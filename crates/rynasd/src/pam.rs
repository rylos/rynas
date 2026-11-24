//! Password check through PAM, plus the user's groups.
//!
//! The PAM service is `rynas` (`/etc/pam.d/rynas`, see `packaging/pam.d/rynas`), falling back
//! to `login` when that file does not exist. `pam_unix` reads `/etc/shadow`, which needs root:
//! a non-root `rynasd` (development) can only verify the password of the user it runs as,
//! through the setuid helper `unix_chkpwd`. Every other user is refused, which is safe.

use std::path::Path;
use std::sync::Mutex;

use pam_client::conv_mock::Conversation;
use pam_client::{Context, Flag};
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};

use crate::groups::user_groups;

pub const REALM: &str = "pam";

/// PAM modules are not guaranteed to be thread-safe: one conversation at a time, logins and
/// account checks alike.
static PAM_LOCK: Mutex<()> = Mutex::new(());

pub struct PamAuthenticator {
    service: String,
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

/// Whether `user` may still sign in, asked without a password (blocking): the account exists,
/// its password is not locked (`!` in `/etc/shadow`), and PAM account management on `service`
/// accepts it (expired accounts, `pam_access`, `pam_time`…). A paired mobile device's refresh
/// and the devices' periodic check use it.
pub fn account_state(service: &str, user: &str) -> crate::devices::Account {
    use crate::devices::Account;
    if user_groups(user).is_none() {
        return Account::Gone;
    }
    if shadow_locked(user) {
        return Account::Disabled;
    }
    let _g = PAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let conv = Conversation::new();
    let ok = Context::new(service, Some(user), conv)
        .and_then(|mut ctx| ctx.acct_mgmt(Flag::NONE))
        .map_err(|e| tracing::debug!("PAM account check of {user}: {e}"))
        .is_ok();
    if ok { Account::Ok } else { Account::Disabled }
}

/// The password field of `user` in `/etc/shadow` starts with `!` (`passwd -l`, `usermod -L`).
fn shadow_locked(user: &str) -> bool {
    std::fs::read_to_string("/etc/shadow")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.split(':').next() == Some(user))
                .map(|l| l.split(':').nth(1).unwrap_or("").starts_with('!'))
        })
        .unwrap_or(false)
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
            let service = self.service.clone();
            let (user, pw) = (username.to_owned(), password.to_owned());
            tokio::task::spawn_blocking(move || {
                let _g = PAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
