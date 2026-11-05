//! Password check through PAM, plus the user's groups.
//!
//! The PAM service is `rynas` (`/etc/pam.d/rynas`, see `packaging/pam.d/rynas`), falling back
//! to `login` when that file does not exist. `pam_unix` reads `/etc/shadow`, which needs root:
//! a non-root `rynasd` (development) can only verify the password of the user it runs as,
//! through the setuid helper `unix_chkpwd`. Every other user is refused, which is safe.

use std::path::Path;
use std::sync::{Arc, Mutex};

use pam_client::conv_mock::Conversation;
use pam_client::{Context, Flag};
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};

use crate::groups::user_groups;

pub const REALM: &str = "pam";

pub struct PamAuthenticator {
    service: String,
    /// PAM modules are not guaranteed to be thread-safe: one conversation at a time.
    lock: Arc<Mutex<()>>,
}

/// `preferred` if `/etc/pam.d/<preferred>` exists, else `login`.
pub fn resolve_service(preferred: &str) -> String {
    if Path::new("/etc/pam.d").join(preferred).exists() {
        preferred.to_owned()
    } else {
        tracing::warn!("/etc/pam.d/{preferred} not found: using the \"login\" PAM service");
        "login".to_owned()
    }
}

impl PamAuthenticator {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn service(&self) -> &str {
        &self.service
    }
}

/// Blocking: PAM modules sleep on failure (`pam_faildelay`, `pam_unix`).
fn pam_check(service: &str, user: &str, password: &str) -> Result<(), String> {
    let conv = Conversation::with_credentials(user, password);
    let mut ctx = Context::new(service, Some(user), conv).map_err(|e| e.to_string())?;
    ctx.authenticate(Flag::DISALLOW_NULL_AUTHTOK)
        .map_err(|e| format!("authenticate: {e}"))?;
    // expired or locked accounts, pam_access, pam_time...
    ctx.acct_mgmt(Flag::DISALLOW_NULL_AUTHTOK)
        .map_err(|e| format!("account: {e}"))?;
    Ok(())
}

impl Authenticator for PamAuthenticator {
    fn realm(&self) -> &str {
        REALM
    }

    fn authenticate<'a>(
        &'a self,
        username: &'a str,
        password: &'a str,
    ) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async move {
            let (service, lock) = (self.service.clone(), self.lock.clone());
            let (user, pw) = (username.to_owned(), password.to_owned());
            tokio::task::spawn_blocking(move || {
                let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
                match pam_check(&service, &user, &pw) {
                    Ok(()) => user_groups(&user)
                        .map(|groups| Identity {
                            username: user.clone(),
                            groups,
                        })
                        .ok_or_else(|| {
                            AuthError::Unavailable(format!("no passwd entry for {user}"))
                        }),
                    Err(e) => {
                        tracing::debug!("PAM: {e}");
                        Err(AuthError::InvalidCredentials)
                    }
                }
            })
            .await
            .map_err(|e| AuthError::Unavailable(e.to_string()))?
        })
    }
}
