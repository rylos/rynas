//! TOTP (RFC 6238: HMAC-SHA1, 6 digits, 30 s, ±1 step) with replay protection and recovery
//! codes.
//!
//! Per user a [`TotpRecord`] holds the confirmed secret, a pending one during enrollment, the
//! last accepted time step (a code is accepted once) and the SHA-256 hashes of the recovery
//! codes. Recovery codes carry 80 random bits each, so a fast hash is enough: there is no
//! dictionary to try. Records live in a [`TotpStorage`]: files (`rynasd`, 0600 in a 0700
//! directory, atomic writes) or memory (tests, internal realm without a state directory).

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rynas_api_types::TotpEnrollment;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use totp_rs::{Builder, Secret, Totp};

use crate::backend::{BoxFuture, TotpBackend, TotpError, TotpStatus};
use crate::{Clock, random_bytes, valid_username};

pub const DIGITS: u32 = 6;
pub const PERIOD_S: u32 = 30;
pub const RECOVERY_CODES: usize = 10;
/// Crockford base32 (no I, L, O, U): 32 symbols, 5 bits each.
const RECOVERY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TotpRecord {
    /// Confirmed secret (base32): TOTP is enabled when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Secret waiting for its first code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
    /// Hex SHA-256 of the normalized unused recovery codes.
    #[serde(default)]
    pub recovery: Vec<String>,
    /// Last accepted time step: codes of this step or older are refused.
    #[serde(default)]
    pub last_step: u64,
}

pub trait TotpStorage: Send + Sync + 'static {
    fn load(&self, user: &str) -> io::Result<Option<TotpRecord>>;
    fn save(&self, user: &str, rec: &TotpRecord) -> io::Result<()>;
    fn delete(&self, user: &str) -> io::Result<()>;
}

#[derive(Default)]
pub struct MemoryStorage(Mutex<HashMap<String, TotpRecord>>);

impl TotpStorage for MemoryStorage {
    fn load(&self, user: &str) -> io::Result<Option<TotpRecord>> {
        Ok(self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(user)
            .cloned())
    }
    fn save(&self, user: &str, rec: &TotpRecord) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(user.into(), rec.clone());
        Ok(())
    }
    fn delete(&self, user: &str) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(user);
        Ok(())
    }
}

/// One JSON file per user in `dir` (created 0700); files are 0600.
pub struct FileStorage {
    dir: PathBuf,
}

impl FileStorage {
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        // an existing directory keeps its mode: tighten it
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        Ok(Self { dir })
    }

    fn path(&self, user: &str) -> io::Result<PathBuf> {
        if !valid_username(user) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid user name",
            ));
        }
        Ok(self.dir.join(format!("{user}.json")))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl TotpStorage for FileStorage {
    fn load(&self, user: &str) -> io::Result<Option<TotpRecord>> {
        match fs::read(self.path(user)?) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn save(&self, user: &str, rec: &TotpRecord) -> io::Result<()> {
        let path = self.path(user)?;
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(rec).map_err(io::Error::other)?;
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        fs::File::open(&self.dir)?.sync_all()
    }

    fn delete(&self, user: &str) -> io::Result<()> {
        match fs::remove_file(self.path(user)?) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Codes: spaces and dashes are ignored, letters are case-insensitive.
fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(char::to_uppercase)
        .collect()
}

fn hash_recovery(normalized: &str) -> String {
    // Crockford decoding: O reads as 0, I and L as 1
    let canonical: String = normalized
        .chars()
        .map(|c| match c {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect();
    let d = Sha256::digest(canonical.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn new_recovery_code() -> String {
    let bytes = random_bytes::<16>();
    let chars: String = bytes
        .iter()
        .map(|b| RECOVERY_ALPHABET[(b & 31) as usize] as char)
        .collect();
    format!(
        "{}-{}-{}-{}",
        &chars[0..4],
        &chars[4..8],
        &chars[8..12],
        &chars[12..16]
    )
}

fn totp_for(secret_b32: &str) -> Result<Totp, TotpError> {
    let secret = Secret::try_from_base32(secret_b32)
        .map_err(|_| TotpError::Unavailable("corrupt TOTP secret".into()))?;
    Builder::new()
        .with_secret(secret)
        .build()
        .map_err(|e| TotpError::Unavailable(format!("bad TOTP secret: {e:?}")))
}

/// RFC 3986 percent-encoding of everything but unreserved characters.
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub struct TotpManager {
    storage: Box<dyn TotpStorage>,
    clock: Arc<dyn Clock>,
    issuer: String,
    /// Serializes read-modify-write cycles.
    lock: Mutex<()>,
}

impl TotpManager {
    pub fn new(
        storage: impl TotpStorage,
        clock: Arc<dyn Clock>,
        issuer: impl Into<String>,
    ) -> Self {
        Self {
            storage: Box::new(storage),
            clock,
            issuer: issuer.into(),
            lock: Mutex::new(()),
        }
    }

    fn load(&self, user: &str) -> Result<TotpRecord, TotpError> {
        if !valid_username(user) {
            return Err(TotpError::InvalidUser);
        }
        self.storage
            .load(user)
            .map(Option::unwrap_or_default)
            .map_err(|e| TotpError::Unavailable(e.to_string()))
    }

    fn save(&self, user: &str, rec: &TotpRecord) -> Result<(), TotpError> {
        let r = if rec.secret.is_none() && rec.pending.is_none() {
            self.storage.delete(user)
        } else {
            self.storage.save(user, rec)
        };
        r.map_err(|e| TotpError::Unavailable(e.to_string()))
    }

    fn now_s(&self) -> u64 {
        self.clock.now_ms() / 1000
    }

    /// Accept `code` for `secret` if it matches a step newer than `rec.last_step`.
    fn check_totp(
        &self,
        secret: &str,
        code: &str,
        rec: &mut TotpRecord,
    ) -> Result<bool, TotpError> {
        let code = normalize(code);
        if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(false);
        }
        match totp_for(secret)?.check(&code, self.now_s()) {
            Some(step) if step > rec.last_step => {
                rec.last_step = step;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Consume a matching recovery code.
    fn check_recovery(code: &str, rec: &mut TotpRecord) -> bool {
        let h = hash_recovery(&normalize(code));
        let mut found = None;
        for (i, stored) in rec.recovery.iter().enumerate() {
            if crate::ct_eq(stored, &h) {
                found = Some(i);
            }
        }
        if let Some(i) = found {
            rec.recovery.remove(i);
        }
        found.is_some()
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn status_sync(&self, user: &str) -> Result<TotpStatus, TotpError> {
        let rec = self.load(user)?;
        Ok(TotpStatus {
            enabled: rec.secret.is_some(),
            pending: rec.pending.is_some(),
        })
    }

    pub fn enroll_sync(&self, user: &str, account: &str) -> Result<TotpEnrollment, TotpError> {
        let _g = self.guard();
        let mut rec = self.load(user)?;
        if rec.secret.is_some() {
            return Err(TotpError::AlreadyEnabled);
        }
        let secret = Secret::from(random_bytes::<20>().to_vec()).to_base32();
        let secret = secret.trim_end_matches('=').to_string();
        rec.pending = Some(secret.clone());
        self.save(user, &rec)?;
        let label = format!("{}:{}", pct(&self.issuer), pct(account));
        Ok(TotpEnrollment {
            otpauth_uri: format!(
                "otpauth://totp/{label}?secret={secret}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={PERIOD_S}",
                pct(&self.issuer)
            ),
            secret,
            issuer: self.issuer.clone(),
            account: account.into(),
            digits: DIGITS,
            period_s: PERIOD_S,
        })
    }

    pub fn confirm_sync(&self, user: &str, code: &str) -> Result<Vec<String>, TotpError> {
        let _g = self.guard();
        let mut rec = self.load(user)?;
        if rec.secret.is_some() {
            return Err(TotpError::AlreadyEnabled);
        }
        let pending = rec.pending.clone().ok_or(TotpError::NotEnrolled)?;
        if !self.check_totp(&pending, code, &mut rec)? {
            return Err(TotpError::InvalidCode);
        }
        let codes: Vec<String> = (0..RECOVERY_CODES).map(|_| new_recovery_code()).collect();
        rec.recovery = codes.iter().map(|c| hash_recovery(&normalize(c))).collect();
        rec.secret = Some(pending);
        rec.pending = None;
        self.save(user, &rec)?;
        Ok(codes)
    }

    /// Check a TOTP code or consume a recovery code of an enabled account.
    pub fn verify_sync(&self, user: &str, code: &str) -> Result<(), TotpError> {
        let _g = self.guard();
        let mut rec = self.load(user)?;
        let secret = rec.secret.clone().ok_or(TotpError::NotEnrolled)?;
        let ok = self.check_totp(&secret, code, &mut rec)? || Self::check_recovery(code, &mut rec);
        if !ok {
            return Err(TotpError::InvalidCode);
        }
        self.save(user, &rec)
    }

    pub fn disable_sync(&self, user: &str, code: &str) -> Result<(), TotpError> {
        let _g = self.guard();
        let mut rec = self.load(user)?;
        let secret = rec.secret.clone().ok_or(TotpError::NotEnrolled)?;
        if !(self.check_totp(&secret, code, &mut rec)? || Self::check_recovery(code, &mut rec)) {
            return Err(TotpError::InvalidCode);
        }
        self.save(user, &TotpRecord::default())
    }
}

impl TotpBackend for TotpManager {
    fn status<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<TotpStatus, TotpError>> {
        Box::pin(async move { self.status_sync(user) })
    }
    fn enroll<'a>(
        &'a self,
        user: &'a str,
        account: &'a str,
    ) -> BoxFuture<'a, Result<TotpEnrollment, TotpError>> {
        Box::pin(async move { self.enroll_sync(user, account) })
    }
    fn confirm<'a>(
        &'a self,
        user: &'a str,
        code: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, TotpError>> {
        Box::pin(async move { self.confirm_sync(user, code) })
    }
    fn disable<'a>(&'a self, user: &'a str, code: &'a str) -> BoxFuture<'a, Result<(), TotpError>> {
        Box::pin(async move { self.disable_sync(user, code) })
    }
    fn verify<'a>(&'a self, user: &'a str, code: &'a str) -> BoxFuture<'a, Result<(), TotpError>> {
        Box::pin(async move { self.verify_sync(user, code) })
    }
}

/// The current code for a base32 secret at `unix_s` (tests and tooling).
pub fn code_at(secret_b32: &str, unix_s: u64) -> Option<String> {
    totp_for(secret_b32)
        .ok()
        .map(|t| t.generate(unix_s).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManualClock;

    const T0: u64 = 1_800_000_000_000;

    fn mgr(clock: Arc<ManualClock>) -> TotpManager {
        TotpManager::new(MemoryStorage::default(), clock, "rynas")
    }

    #[test]
    fn rfc6238_vector() {
        // RFC 6238 appendix B, SHA1, secret "12345678901234567890", T = 59 s -> 94287082 (8 digits)
        let b32 = Secret::from(b"12345678901234567890".to_vec()).to_base32();
        assert_eq!(code_at(&b32, 59).unwrap(), "287082");
        assert_eq!(code_at(&b32, 1111111109).unwrap(), "081804");
    }

    #[test]
    fn enroll_confirm_verify_disable() {
        let clock = Arc::new(ManualClock::new(T0));
        let m = mgr(clock.clone());
        assert_eq!(m.status_sync("bob").unwrap(), TotpStatus::default());
        let e = m.enroll_sync("bob", "bob@nas").unwrap();
        assert_eq!(e.secret.len(), 32);
        assert!(
            e.otpauth_uri
                .starts_with("otpauth://totp/rynas:bob%40nas?secret=")
        );
        assert!(m.status_sync("bob").unwrap().pending);
        assert_eq!(m.verify_sync("bob", "000000"), Err(TotpError::NotEnrolled));
        assert_eq!(m.confirm_sync("bob", "12345"), Err(TotpError::InvalidCode));

        let now = |c: &ManualClock| code_at(&e.secret, c.now_ms() / 1000).unwrap();
        let codes = m.confirm_sync("bob", &now(&clock)).unwrap();
        assert_eq!(codes.len(), RECOVERY_CODES);
        assert_eq!(codes[0].len(), 19);
        assert!(m.status_sync("bob").unwrap().enabled);
        assert_eq!(m.enroll_sync("bob", "x"), Err(TotpError::AlreadyEnabled));

        // the code used to confirm cannot be replayed
        assert_eq!(
            m.verify_sync("bob", &now(&clock)),
            Err(TotpError::InvalidCode)
        );
        clock.advance_ms(30_000);
        assert!(m.verify_sync("bob", &now(&clock)).is_ok());
        assert_eq!(
            m.verify_sync("bob", &now(&clock)),
            Err(TotpError::InvalidCode)
        );
        // one step of skew is accepted, an older code is not
        let prev = code_at(&e.secret, clock.now_ms() / 1000 - 60).unwrap();
        clock.advance_ms(30_000);
        assert_eq!(m.verify_sync("bob", &prev), Err(TotpError::InvalidCode));

        // recovery codes: case and dashes do not matter, each works once
        let rc = codes[3].to_lowercase().replace('-', " ");
        assert!(m.verify_sync("bob", &rc).is_ok());
        assert_eq!(m.verify_sync("bob", &codes[3]), Err(TotpError::InvalidCode));

        assert_eq!(m.disable_sync("bob", "999999"), Err(TotpError::InvalidCode));
        assert!(m.disable_sync("bob", &codes[0]).is_ok());
        assert_eq!(m.status_sync("bob").unwrap(), TotpStatus::default());
        assert_eq!(m.verify_sync("../x", "1"), Err(TotpError::InvalidUser));
    }

    #[test]
    fn file_storage_is_private_and_atomic() {
        let dir = std::env::temp_dir().join(format!("rynas-auth-totp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let st = FileStorage::new(&dir).unwrap();
        let rec = TotpRecord {
            secret: Some("ABC".into()),
            ..Default::default()
        };
        st.save("bob", &rec).unwrap();
        assert_eq!(st.load("bob").unwrap(), Some(rec));
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("bob.json")), 0o600);
        assert!(st.save("../evil", &TotpRecord::default()).is_err());
        st.delete("bob").unwrap();
        st.delete("bob").unwrap();
        assert_eq!(st.load("bob").unwrap(), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
