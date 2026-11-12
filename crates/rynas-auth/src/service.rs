//! The login flow: rate limit, password, role, TOTP, session.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rynas_api_types::{LoginRequest, TotpEnrollment, UserInfo};

use crate::backend::{AuthError, Authenticator, RolePolicy, TotpBackend, TotpError};
use crate::ratelimit::{LoginLimiter, RateLimitConfig};
use crate::session::{SessionConfig, SessionData, SessionId, SessionStore};
use crate::{Clock, SystemClock, log_safe, random_bytes, valid_username};

/// A password backend and the TOTP store of its users.
#[derive(Clone)]
pub struct Backend {
    pub authenticator: Arc<dyn Authenticator>,
    pub totp: Arc<dyn TotpBackend>,
}

#[derive(Clone)]
pub struct AuthConfig {
    pub session: SessionConfig,
    pub rate: RateLimitConfig,
    pub policy: RolePolicy,
    /// Failed logins are answered no sooner than this after the request arrived (plus up
    /// to 100 ms of jitter), whatever the backend took.
    pub failure_floor: Duration,
    /// Host name in the TOTP account label ("user@host").
    pub host: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            session: SessionConfig::default(),
            rate: RateLimitConfig::default(),
            policy: RolePolicy::default(),
            failure_floor: Duration::from_millis(500),
            host: "rynas".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginError {
    InvalidCredentials,
    TotpRequired,
    InvalidTotp,
    /// A login that needs TOTP (the restore TUI's) by an account that has not turned it on.
    TotpNotEnabled,
    RateLimited(Duration),
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TotpOpError {
    Totp(TotpError),
    RateLimited(Duration),
    /// The session's realm has no backend any more (configuration changed).
    NoBackend,
}

impl From<TotpError> for TotpOpError {
    fn from(e: TotpError) -> Self {
        Self::Totp(e)
    }
}

pub struct AuthService {
    backends: Vec<Backend>,
    sessions: SessionStore,
    limiter: LoginLimiter,
    policy: RolePolicy,
    failure_floor: Duration,
    host: String,
}

impl AuthService {
    /// Backends are tried in order: a backend answering "unknown user" passes to the next.
    pub fn new(backends: Vec<Backend>, cfg: AuthConfig) -> Self {
        Self::with_clock(backends, cfg, Arc::new(SystemClock))
    }

    pub fn with_clock(backends: Vec<Backend>, cfg: AuthConfig, clock: Arc<dyn Clock>) -> Self {
        Self {
            backends,
            sessions: SessionStore::new(cfg.session, clock.clone()),
            limiter: LoginLimiter::new(cfg.rate, clock),
            policy: cfg.policy,
            failure_floor: cfg.failure_floor,
            host: cfg.host,
        }
    }

    pub fn sessions(&self) -> &SessionStore {
        &self.sessions
    }

    fn backend(&self, realm: &str) -> Option<&Backend> {
        self.backends
            .iter()
            .find(|b| b.authenticator.realm() == realm)
    }

    async fn fail(&self, started: Instant, ip: IpAddr, user: &str, e: LoginError) -> LoginError {
        self.limiter.record_failure(ip, user);
        if !self.failure_floor.is_zero() {
            let jitter = Duration::from_millis(u64::from(random_bytes::<1>()[0]) * 100 / 255);
            tokio::time::sleep_until((started + self.failure_floor + jitter).into()).await;
        }
        e
    }

    /// Returns the cookie token and the new session.
    pub async fn login(
        &self,
        ip: IpAddr,
        req: &LoginRequest,
    ) -> Result<(String, SessionData), LoginError> {
        let (info, totp) = self.verify(ip, req, false).await?;
        Ok(self.sessions.create(info, totp))
    }

    /// Checks an administrator's password and TOTP code without opening a session: the PC
    /// restore TUI's second login (D-PCBACKUP). An account without TOTP is refused even with
    /// the right password, so a password alone never opens a PC's backups from a live system.
    pub async fn verify_admin_totp(&self, ip: IpAddr, req: &LoginRequest) -> Result<UserInfo, LoginError> {
        self.verify(ip, req, true).await.map(|(info, _)| info)
    }

    /// The checks of a login: user, password, group, then TOTP (required by the account, or by
    /// `need_totp`). Returns the user and whether TOTP was checked.
    async fn verify(
        &self,
        ip: IpAddr,
        req: &LoginRequest,
        need_totp: bool,
    ) -> Result<(UserInfo, bool), LoginError> {
        let started = Instant::now();
        let user = req.username.as_str();
        self.limiter
            .check(ip, user)
            .map_err(LoginError::RateLimited)?;
        if !valid_username(user) || req.password.is_empty() || req.password.len() > 4096 {
            return Err(self
                .fail(started, ip, user, LoginError::InvalidCredentials)
                .await);
        }

        let mut found = None;
        let mut unavailable = Vec::new();
        let mut answered = false;
        for b in &self.backends {
            match b.authenticator.authenticate(user, &req.password).await {
                Ok(id) => {
                    found = Some((b, id));
                    break;
                }
                Err(AuthError::UnknownUser) => answered = true,
                Err(AuthError::InvalidCredentials) => {
                    answered = true;
                    break;
                }
                Err(AuthError::Unavailable(e)) => {
                    tracing::error!(
                        "authentication backend {} unavailable: {e}",
                        b.authenticator.realm()
                    );
                    unavailable.push(e);
                }
            }
        }
        let Some((backend, identity)) = found else {
            if !answered && !unavailable.is_empty() || self.backends.is_empty() {
                return Err(LoginError::Unavailable(unavailable.join("; ")));
            }
            return Err(self
                .fail(started, ip, user, LoginError::InvalidCredentials)
                .await);
        };
        let realm = backend.authenticator.realm().to_owned();

        let Some(role) = self.policy.role(&identity) else {
            tracing::warn!(
                target: "rynas_auth",
                "user {} is not in any of the groups {:?}",
                log_safe(&identity.username),
                self.policy.admin_groups
            );
            return Err(self
                .fail(started, ip, user, LoginError::InvalidCredentials)
                .await);
        };

        let status = backend
            .totp
            .status(&identity.username)
            .await
            .map_err(|e| LoginError::Unavailable(e.to_string()))?;
        if need_totp && !status.enabled {
            return Err(self.fail(started, ip, user, LoginError::TotpNotEnabled).await);
        }
        if status.enabled {
            match req.totp.as_deref().filter(|c| !c.trim().is_empty()) {
                None => return Err(LoginError::TotpRequired),
                Some(code) => match backend.totp.verify(&identity.username, code).await {
                    Ok(()) => {}
                    Err(TotpError::InvalidCode | TotpError::NotEnrolled) => {
                        return Err(self.fail(started, ip, user, LoginError::InvalidTotp).await);
                    }
                    Err(e) => return Err(LoginError::Unavailable(e.to_string())),
                },
            }
        }

        self.limiter.record_success(ip, user);
        tracing::info!(
            target: "rynas_auth",
            "login of {} ({realm}) from {ip}",
            log_safe(&identity.username)
        );
        let info = UserInfo {
            username: identity.username,
            realm,
            role,
        };
        Ok((info, status.enabled))
    }

    pub fn logout(&self, id: &SessionId) {
        if let Some(s) = self.sessions.get_by_id(id, false) {
            tracing::info!(target: "rynas_auth", "logout of {}", log_safe(&s.user.username));
        }
        self.sessions.remove(id);
    }

    fn totp_backend(&self, s: &SessionData) -> Result<&Arc<dyn TotpBackend>, TotpOpError> {
        self.backend(&s.user.realm)
            .map(|b| &b.totp)
            .ok_or(TotpOpError::NoBackend)
    }

    pub async fn totp_enroll(&self, s: &SessionData) -> Result<TotpEnrollment, TotpOpError> {
        let account = format!("{}@{}", s.user.username, self.host);
        Ok(self
            .totp_backend(s)?
            .enroll(&s.user.username, &account)
            .await?)
    }

    /// Enables TOTP; the user's other sessions (opened without the second factor) end.
    pub async fn totp_confirm(
        &self,
        ip: IpAddr,
        s: &SessionData,
        code: &str,
    ) -> Result<Vec<String>, TotpOpError> {
        let user = &s.user.username;
        self.limiter
            .check(ip, user)
            .map_err(TotpOpError::RateLimited)?;
        match self.totp_backend(s)?.confirm(user, code).await {
            Ok(codes) => {
                self.sessions.set_totp_enabled(user, &s.user.realm, true);
                self.sessions
                    .remove_user_except(user, &s.user.realm, Some(&s.id));
                tracing::info!(target: "rynas_auth", "TOTP enabled for {}", log_safe(user));
                Ok(codes)
            }
            Err(TotpError::InvalidCode) => {
                self.limiter.record_failure(ip, user);
                Err(TotpError::InvalidCode.into())
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn totp_disable(
        &self,
        ip: IpAddr,
        s: &SessionData,
        code: &str,
    ) -> Result<(), TotpOpError> {
        let user = &s.user.username;
        self.limiter
            .check(ip, user)
            .map_err(TotpOpError::RateLimited)?;
        match self.totp_backend(s)?.disable(user, code).await {
            Ok(()) => {
                self.sessions.set_totp_enabled(user, &s.user.realm, false);
                tracing::info!(target: "rynas_auth", "TOTP disabled for {}", log_safe(user));
                Ok(())
            }
            Err(TotpError::InvalidCode) => {
                self.limiter.record_failure(ip, user);
                Err(TotpError::InvalidCode.into())
            }
            Err(e) => Err(e.into()),
        }
    }
}
