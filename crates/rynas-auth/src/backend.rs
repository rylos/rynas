//! Pluggable authentication backends.
//!
//! An [`Authenticator`] checks a password and returns the user's groups; a [`TotpBackend`]
//! keeps the user's TOTP secret. In production both live in the root daemon `rynasd` (PAM
//! needs root to read `/etc/shadow`, TOTP secrets are root-only files) and `rynas-web` reaches
//! them over a Unix socket; the internal realm and tests use local implementations.

use std::future::Future;
use std::pin::Pin;

use rynas_api_types::{Role, TotpEnrollment};
use serde::{Deserialize, Serialize};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A successfully authenticated user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub username: String,
    /// Names of all the groups of the user (primary and supplementary).
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// This backend does not know the user: the next backend may (internal realm only;
    /// PAM cannot tell an unknown user from a wrong password).
    UnknownUser,
    InvalidCredentials,
    /// The backend could not be asked (rynasd down, I/O error...).
    Unavailable(String),
}

pub trait Authenticator: Send + Sync + 'static {
    /// Realm name reported in [`rynas_api_types::UserInfo::realm`], e.g. "pam" or "rynas".
    fn realm(&self) -> &str;
    fn authenticate<'a>(
        &'a self,
        username: &'a str,
        password: &'a str,
    ) -> BoxFuture<'a, Result<Identity, AuthError>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TotpStatus {
    pub enabled: bool,
    /// An enrollment is waiting for its confirmation code.
    pub pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TotpError {
    AlreadyEnabled,
    NotEnrolled,
    InvalidCode,
    InvalidUser,
    Unavailable(String),
}

impl std::fmt::Display for TotpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyEnabled => f.write_str("TOTP is already enabled"),
            Self::NotEnrolled => f.write_str("no TOTP enrollment for this user"),
            Self::InvalidCode => f.write_str("invalid TOTP or recovery code"),
            Self::InvalidUser => f.write_str("invalid user name"),
            Self::Unavailable(e) => write!(f, "TOTP storage unavailable: {e}"),
        }
    }
}

pub trait TotpBackend: Send + Sync + 'static {
    fn status<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<TotpStatus, TotpError>>;
    /// Start (or restart) an enrollment; fails with `AlreadyEnabled` if TOTP is on.
    fn enroll<'a>(
        &'a self,
        user: &'a str,
        account: &'a str,
    ) -> BoxFuture<'a, Result<TotpEnrollment, TotpError>>;
    /// Check a code against the pending secret, enable TOTP and return fresh recovery codes.
    fn confirm<'a>(
        &'a self,
        user: &'a str,
        code: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, TotpError>>;
    /// Disable TOTP; `code` is a current TOTP code or a recovery code.
    fn disable<'a>(&'a self, user: &'a str, code: &'a str) -> BoxFuture<'a, Result<(), TotpError>>;
    /// Check a login code (TOTP or recovery code, which is then consumed).
    fn verify<'a>(&'a self, user: &'a str, code: &'a str) -> BoxFuture<'a, Result<(), TotpError>>;
}

/// Maps groups to a rynas role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolePolicy {
    /// Members of any of these groups are administrators.
    pub admin_groups: Vec<String>,
}

impl Default for RolePolicy {
    fn default() -> Self {
        Self {
            admin_groups: vec!["rynas-admin".into(), "sudo".into()],
        }
    }
}

impl RolePolicy {
    pub fn role(&self, identity: &Identity) -> Option<Role> {
        identity
            .groups
            .iter()
            .any(|g| self.admin_groups.iter().any(|a| a == g))
            .then_some(Role::Admin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_by_group() {
        let p = RolePolicy::default();
        let id = |g: &[&str]| Identity {
            username: "u".into(),
            groups: g.iter().map(|s| s.to_string()).collect(),
        };
        assert_eq!(p.role(&id(&["users", "sudo"])), Some(Role::Admin));
        assert_eq!(p.role(&id(&["rynas-admin"])), Some(Role::Admin));
        assert_eq!(p.role(&id(&["users", "wheel"])), None);
        assert_eq!(p.role(&id(&[])), None);
    }
}
