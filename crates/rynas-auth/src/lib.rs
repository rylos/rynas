//! rynas authentication.
//!
//! - [`session`]: in-memory sessions (256-bit random tokens, idle and absolute timeouts) with a
//!   CSRF token each;
//! - [`ratelimit`]: failed-login throttling per client IP and per user name with exponential
//!   backoff, logged in a fail2ban-friendly format;
//! - [`totp`]: RFC 6238 TOTP with replay protection and hashed recovery codes;
//! - [`realm`]: the internal "rynas" realm (users file with argon2 hashes);
//! - [`backend`]: the [`backend::Authenticator`] and [`backend::TotpBackend`] traits, so that
//!   PAM (in the root daemon `rynasd`) can be swapped for a fake in tests;
//! - [`service`]: the login flow that ties everything together;
//! - [`ipc`]: the internal protocol between `rynas-web` and `rynasd`.

pub mod backend;
pub mod ipc;
pub mod ratelimit;
pub mod realm;
pub mod service;
pub mod session;
pub mod totp;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;

pub use backend::{AuthError, Authenticator, Identity, RolePolicy, TotpBackend, TotpStatus};
pub use service::{AuthConfig, AuthService, Backend, LoginError};

/// Source of wall-clock time in milliseconds since the Unix epoch (injectable for tests).
pub trait Clock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// A clock that only moves when told to.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn new(now_ms: u64) -> Self {
        Self(AtomicU64::new(now_ms))
    }
    pub fn advance_ms(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
    pub fn set_ms(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// `N` bytes from the operating system CSPRNG.
///
/// # Panics
/// If the OS has no random source, which on Linux means `getrandom(2)` is broken: there is
/// nothing safe to fall back to.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the operating system random source failed");
    b
}

/// A 256-bit random token, base64url without padding (43 characters).
pub fn random_token() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

/// Constant-time string comparison.
pub fn ct_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// Accepted user names: 1-64 characters of `[A-Za-z0-9._-]`, not starting with `-` or `.`.
///
/// Stricter than what Linux allows on purpose: the name ends up in file names (TOTP state),
/// in PAM calls and in logs.
pub fn valid_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['-', '.'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A user-supplied string made safe for a single log line: no control characters or
/// whitespace (which could fake fields for fail2ban), at most 64 characters.
pub fn log_safe(s: &str) -> String {
    let mut out: String = s
        .chars()
        .take(64)
        .map(|c| {
            if c.is_control() || c.is_whitespace() {
                '_'
            } else {
                c
            }
        })
        .collect();
    if out.is_empty() {
        out.push('-');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames() {
        for ok in ["alice", "a", "rynas-admin", "john.doe", "u_1", "Admin"] {
            assert!(valid_username(ok), "{ok}");
        }
        let long = "a".repeat(65);
        for bad in [
            "",
            "-x",
            ".x",
            "a/b",
            "../etc",
            "a b",
            "é",
            "a\n",
            long.as_str(),
        ] {
            assert!(!valid_username(bad), "{bad:?}");
        }
    }

    #[test]
    fn log_safe_strips_injection() {
        assert_eq!(log_safe("bob from 1.2.3.4\nx"), "bob_from_1.2.3.4_x");
        assert_eq!(log_safe(""), "-");
        assert_eq!(log_safe(&"x".repeat(100)).len(), 64);
    }

    #[test]
    fn tokens_are_random_and_long() {
        let a = random_token();
        assert_eq!(a.len(), 43);
        assert_ne!(a, random_token());
        assert!(ct_eq(&a, &a.clone()));
        assert!(!ct_eq(&a, "x"));
    }
}
