//! The internal "rynas" realm: web-only users from a TOML file with argon2id hashes.
//!
//! ```toml
//! [[user]]
//! name = "admin"
//! hash = "$argon2id$v=19$m=19456,t=2,p=1$..."   # rynas-web hash-password
//! groups = ["rynas-admin"]
//! ```
//!
//! An unknown user costs the same argon2 work as a known one (verification against a dummy
//! hash), so response times do not reveal which names exist.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use serde::Deserialize;

use crate::backend::{AuthError, Authenticator, BoxFuture, Identity};
use crate::{random_token, valid_username};

pub const REALM: &str = "rynas";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RealmUser {
    pub name: String,
    /// PHC string, argon2id recommended.
    pub hash: String,
    #[serde(default)]
    pub groups: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsersFile {
    #[serde(default)]
    user: Vec<RealmUser>,
}

pub struct Realm {
    users: HashMap<String, RealmUser>,
    dummy_hash: String,
}

/// Hash a password with argon2id and the crate's default (OWASP) parameters.
pub fn hash_password(password: &str) -> Result<String, String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

impl Realm {
    pub fn from_users(users: Vec<RealmUser>) -> Result<Self, String> {
        let mut map = HashMap::new();
        for u in users {
            if !valid_username(&u.name) {
                return Err(format!("invalid user name {:?}", u.name));
            }
            PasswordHash::new(&u.hash).map_err(|e| format!("user {}: bad hash: {e}", u.name))?;
            if map.insert(u.name.clone(), u).is_some() {
                return Err("duplicate user".into());
            }
        }
        Ok(Self {
            users: map,
            dummy_hash: hash_password(&random_token())?,
        })
    }

    pub fn parse(toml_text: &str) -> Result<Self, String> {
        let f: UsersFile = toml::from_str(toml_text).map_err(|e| e.to_string())?;
        Self::from_users(f.user)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn len(&self) -> usize {
        self.users.len()
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    /// Blocking (argon2 takes tens of milliseconds).
    pub fn verify(&self, username: &str, password: &str) -> Result<Identity, AuthError> {
        let (hash, user) = match self.users.get(username) {
            Some(u) => (u.hash.as_str(), Some(u)),
            None => (self.dummy_hash.as_str(), None),
        };
        let ok = PasswordHash::new(hash)
            .map(|h| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &h)
                    .is_ok()
            })
            .unwrap_or(false);
        match (user, ok) {
            (Some(u), true) => Ok(Identity {
                username: u.name.clone(),
                groups: u.groups.clone(),
            }),
            (Some(_), false) => Err(AuthError::InvalidCredentials),
            (None, _) => Err(AuthError::UnknownUser),
        }
    }
}

/// [`Authenticator`] for a [`Realm`]; verification runs on the blocking thread pool.
pub struct RealmAuthenticator {
    realm: Arc<Realm>,
    /// argon2 needs ~19 MiB per verification: one at a time (a login takes tens of ms, and
    /// failed attempts are throttled anyway), so the peak stays at one block.
    permits: tokio::sync::Semaphore,
}

impl RealmAuthenticator {
    pub fn new(realm: Realm) -> Self {
        Self {
            realm: Arc::new(realm),
            permits: tokio::sync::Semaphore::new(1),
        }
    }
}

impl Authenticator for RealmAuthenticator {
    fn realm(&self) -> &str {
        REALM
    }

    fn authenticate<'a>(
        &'a self,
        username: &'a str,
        password: &'a str,
    ) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async move {
            let _permit = self
                .permits
                .acquire()
                .await
                .map_err(|e| AuthError::Unavailable(e.to_string()))?;
            let realm = self.realm.clone();
            let (u, p) = (username.to_owned(), password.to_owned());
            tokio::task::spawn_blocking(move || realm.verify(&u, &p))
                .await
                .map_err(|e| AuthError::Unavailable(e.to_string()))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_verify() {
        let h = hash_password("s3cret").unwrap();
        assert!(h.starts_with("$argon2id$"));
        let realm = Realm::parse(&format!(
            "[[user]]\nname = \"admin\"\nhash = \"{h}\"\ngroups = [\"rynas-admin\"]\n"
        ))
        .unwrap();
        assert_eq!(realm.len(), 1);
        let id = realm.verify("admin", "s3cret").unwrap();
        assert_eq!(id.groups, ["rynas-admin"]);
        assert_eq!(
            realm.verify("admin", "nope"),
            Err(AuthError::InvalidCredentials)
        );
        assert_eq!(realm.verify("ghost", "s3cret"), Err(AuthError::UnknownUser));
    }

    #[test]
    fn rejects_bad_files() {
        assert!(Realm::parse("[[user]]\nname = \"a\"\nhash = \"plain\"\n").is_err());
        assert!(Realm::parse("[[user]]\nname = \"../a\"\nhash = \"x\"\n").is_err());
        assert!(Realm::parse("[[user]]\nname = \"a\"\npassword = \"x\"\n").is_err());
        assert!(Realm::parse("").unwrap().is_empty());
    }
}
