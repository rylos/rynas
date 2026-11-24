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
///
/// Only definite answers revoke: `Gone` for a user NSS cleanly says does not exist,
/// `Disabled` for a `!`/`*` lock in `/etc/shadow` or an expired account (`PAM_ACCT_EXPIRED`).
/// Any other failure (NSS or PAM error, unreadable shadow file) is `Unknown`: the refresh is
/// refused for now and the devices stay.
pub fn account_state(service: &str, user: &str) -> crate::devices::Account {
    use crate::devices::Account;
    match crate::groups::user_groups_checked(user) {
        Ok(Some(_)) => {}
        Ok(None) => return Account::Gone,
        Err(e) => {
            tracing::warn!("account check of {user}: {e}");
            return Account::Unknown;
        }
    }
    match std::fs::read_to_string("/etc/shadow") {
        Ok(t) if shadow_locked(&t, user) => return Account::Disabled,
        Ok(_) => {}
        Err(e) => {
            tracing::warn!("account check of {user}: /etc/shadow: {e}");
            return Account::Unknown;
        }
    }
    let _g = PAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let r = Context::new(service, Some(user), Conversation::new())
        .and_then(|mut ctx| ctx.acct_mgmt(Flag::NONE));
    match r {
        Ok(()) => Account::Ok,
        Err(e) => match e.code() {
            pam_client::ErrorCode::ACCT_EXPIRED => Account::Disabled,
            // a password to change does not end the account's access
            pam_client::ErrorCode::NEW_AUTHTOK_REQD => Account::Ok,
            _ => {
                tracing::warn!("PAM account check of {user}: {e}");
                Account::Unknown
            }
        },
    }
}

/// Whether the password field of `user` in a shadow file starts with `!` or `*` (`passwd -l`,
/// `usermod -L`, a system account without a password).
pub fn shadow_locked(shadow: &str, user: &str) -> bool {
    shadow
        .lines()
        .find(|l| l.split(':').next() == Some(user))
        .is_some_and(|l| {
            let pw = l.split(':').nth(1).unwrap_or("");
            pw.starts_with('!') || pw.starts_with('*')
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_locks() {
        let shadow = "alice:$y$j9T$x:19000:0:99999:7:::\nbob:!$y$j9T$x:19000::::::\ncarol:*:19000::::::\n";
        assert!(!shadow_locked(shadow, "alice"));
        assert!(shadow_locked(shadow, "bob"));
        assert!(shadow_locked(shadow, "carol"));
        assert!(!shadow_locked(shadow, "dave"), "not in shadow: not locked here");
        assert!(!shadow_locked(shadow, "ali"));
    }
}
