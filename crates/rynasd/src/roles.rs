//! Roles of the users rynas-web forwards (`x-rynas-user`).
//!
//! rynas-web only opens sessions for administrators today and guards every mutating route
//! with its own `require_admin`; rynasd checks again, from the system groups (NSS), before
//! operations that amount to root on the box — today user tasks that run as `root` (create,
//! change, delete, enable/disable, run now), and any whole `scheduler` value that adds,
//! changes or removes one: restoring a configuration revision, importing a `.rynasbak`
//! archive, applying a changeset ([`crate::ops::root_task_guard`]). The same rule as rynas-web's
//! [`rynas_auth::backend::RolePolicy`]: members of the admin groups (`--admin-group`,
//! default `rynas-admin` and `sudo`) are administrators; `root` always is. Users of
//! rynas-web's internal realm (`--users-file`, development) are not system users and are
//! never administrators here.

use axum::http::StatusCode;
use rynas_api_types::error_codes;

use crate::ops::{Error, Res};

#[derive(Debug, Clone)]
pub struct AdminPolicy {
    groups: Vec<String>,
    lookup: fn(&str) -> Option<Vec<String>>,
}

impl Default for AdminPolicy {
    fn default() -> Self {
        Self::new(
            rynas_auth::backend::RolePolicy::default().admin_groups,
            crate::groups::user_groups,
        )
    }
}

impl AdminPolicy {
    /// `lookup` answers the groups of a user (`None`: no such user).
    pub fn new(groups: Vec<String>, lookup: fn(&str) -> Option<Vec<String>>) -> Self {
        Self { groups, lookup }
    }

    pub fn is_admin(&self, user: &str) -> bool {
        user == "root"
            || (self.lookup)(user)
                .is_some_and(|gs| gs.iter().any(|g| self.groups.iter().any(|a| a == g)))
    }

    /// 403 `forbidden` unless `user` is an administrator.
    pub fn require_admin(&self, user: &str, what: &str) -> Res<()> {
        if self.is_admin(user) {
            Ok(())
        } else {
            tracing::warn!("{} refused for {user}: administrators only", what);
            Err(Error::new(
                StatusCode::FORBIDDEN,
                error_codes::FORBIDDEN,
                format!("{what}: administrators only"),
            ))
        }
    }
}

/// What [`admin_only`] guards: the policy and the name used in the refusal.
#[derive(Debug, Clone)]
pub struct AdminGate {
    pub policy: AdminPolicy,
    pub what: &'static str,
}

/// Route layer for whole routers whose reads are as sensitive as their changes (Docker,
/// the HTTPS certificate): the forwarded user (`x-rynas-user`) must be an administrator,
/// reads included; 400 without a user, 403 `forbidden` otherwise. Handlers that change
/// something still check the role themselves and write their audit line.
pub async fn admin_only(
    axum::extract::State(g): axum::extract::State<AdminGate>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match crate::ops::user(req.headers()).and_then(|u| g.policy.require_admin(&u, g.what)) {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admins_from_groups() {
        let p = AdminPolicy::new(vec!["rynas-admin".into()], |u| match u {
            "alice" => Some(vec!["users".into(), "rynas-admin".into()]),
            "bob" => Some(vec!["users".into()]),
            _ => None,
        });
        assert!(p.is_admin("alice") && p.is_admin("root"));
        assert!(!p.is_admin("bob") && !p.is_admin("ghost"));
        let e = p.require_admin("bob", "root task").unwrap_err();
        assert_eq!((e.0, e.1), (StatusCode::FORBIDDEN, error_codes::FORBIDDEN));
    }
}
