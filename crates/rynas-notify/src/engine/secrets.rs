//! Target secrets at rest: an age (X25519) envelope per target, keyed by
//! `<state>/notify/secret.key` (root, 0600, created on first start).
//!
//! The database alone (a copy of `notify.db`, a backup without the key) does not reveal
//! passwords or tokens. The key file sits next to the TOTP secrets and travels only inside
//! the passphrase-encrypted `secrets.age` of a `.rynasbak` export (D-CFGBAK). Secret values
//! never reach the API, the logs or error messages ([`redact`]).

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::str::FromStr;

use age::secrecy::ExposeSecret;

pub struct SecretBox {
    identity: age::x25519::Identity,
    recipient: age::x25519::Recipient,
}

/// Name of the key file inside the notify directory.
pub const KEY_FILE: &str = "secret.key";

impl SecretBox {
    /// Loads the key, or creates it (0600) when it does not exist.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        let path = dir.join(KEY_FILE);
        let identity = match std::fs::read_to_string(&path) {
            Ok(s) => age::x25519::Identity::from_str(s.trim())
                .map_err(|e| std::io::Error::other(format!("{}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let id = age::x25519::Identity::generate();
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?;
                writeln!(f, "{}", id.to_string().expose_secret())?;
                f.sync_all()?;
                id
            }
            Err(e) => return Err(e),
        };
        let recipient = identity.to_public();
        Ok(Self {
            identity,
            recipient,
        })
    }

    pub fn seal(&self, secrets: &BTreeMap<String, String>) -> Result<Vec<u8>, String> {
        let plain = serde_json::to_vec(secrets).map_err(|e| e.to_string())?;
        age::encrypt(&self.recipient, &plain).map_err(|e| e.to_string())
    }

    pub fn open_sealed(&self, sealed: &[u8]) -> Result<BTreeMap<String, String>, String> {
        let plain = age::decrypt(&self.identity, sealed).map_err(|e| e.to_string())?;
        serde_json::from_slice(&plain).map_err(|e| e.to_string())
    }
}

/// `text` with every secret value replaced by `***` (error messages of channels may echo
/// URLs or headers).
pub fn redact(text: &str, secrets: &BTreeMap<String, String>) -> String {
    let mut out = text.to_string();
    for v in secrets.values().filter(|v| v.len() >= 4) {
        out = out.replace(v.as_str(), "***");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn roundtrip_and_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let b = SecretBox::open(dir.path()).unwrap();
        let m: BTreeMap<String, String> = [("password".to_string(), "hunter22".to_string())].into();
        let sealed = b.seal(&m).unwrap();
        assert!(!sealed.windows(8).any(|w| w == b"hunter22"));
        // the same key after a restart
        let b2 = SecretBox::open(dir.path()).unwrap();
        assert_eq!(b2.open_sealed(&sealed).unwrap(), m);
        let mode = std::fs::metadata(dir.path().join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(redact("GET /bothunter22/x", &m), "GET /bot***/x");
    }
}
