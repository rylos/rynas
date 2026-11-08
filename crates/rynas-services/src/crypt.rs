//! Encrypted shared folders (D-CRYPT E1, `docs/phase2/encrypted-folders.md`): gocryptfs
//! from the Debian archive.
//!
//! A folder `<place>/<name>` keeps its ciphertext in `<place>/.rynas-crypt/<name>` (a
//! subvolume on btrfs) and is mounted on `<place>/<name>` (a plain folder, mode 000,
//! immutable) while unlocked. Mounting never happens inside rynasd: unlock starts the
//! transient service `rynas-crypt-<escaped path>` through [`rynas_tasks::Outside::daemon`]
//! with the password as a systemd credential, lock unmounts through [`rynas_tasks::Outside`].
//!
//! Secrets travel on standard input or in the 0600 credential file that lives only for the
//! start of the unit; they are never in argv, the environment, a log or an error. The
//! master key (the recovery key) is generated here and returned once by [`CryptHost::create`].
//!
//! [`HostCrypt`] works on the machine, [`MemoryCrypt`] in tests and `rynasd --dry-system`.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rynas_api_types::{CryptInfo, CryptState, CryptStatus};
use rynas_tasks::{Cmd, CmdOutput, Outside};
use sha2::{Digest, Sha256};

use crate::accounts::Secret;
use crate::host::SubvolumeCreator;

/// Folder below a place that holds the ciphertext directories (0700 root).
pub const CRYPT_DIR: &str = ".rynas-crypt";
pub const GOCRYPTFS: &str = "/usr/bin/gocryptfs";
/// The configuration file of a gocryptfs ciphertext directory.
pub const CONF: &str = "gocryptfs.conf";
/// What `gocryptfs -passwd` leaves behind, wrapped with the **old** password.
pub const CONF_BAK: &str = "gocryptfs.conf.bak";
/// Prefix of the mount units.
pub const UNIT_PREFIX: &str = "rynas-crypt";
/// Credential files that live for the start of a mount unit (tmpfs, 0700 root).
pub const RUN_DIR: &str = "/run/rynas/crypt";
/// Debian package.
pub const PACKAGE: &str = "gocryptfs";
/// Reads the master key of a ciphertext directory with its password (`-dumpmasterkey`):
/// "show the recovery key again". Shipped in the Debian `gocryptfs` package.
pub const XRAY: &str = "/usr/bin/gocryptfs-xray";
/// Password length in characters, the same limit in the UI and the API (at most 1024 bytes
/// of UTF-8: gocryptfs reads up to 2048).
pub const PASSWORD_MIN: usize = 8;
pub const PASSWORD_MAX: usize = 256;
/// `gocryptfs` exit code for a wrong password.
pub const EXIT_WRONG_PASSWORD: i32 = 12;
/// The oldest gocryptfs with `-init -masterkey` (2.5.0; Debian 12 ships 2.3).
pub const MIN_VERSION: (u32, u32) = (2, 5);
/// The key check of a folder (SHA-256 of the master key, see [`key_check`]), in the root of
/// its ciphertext directory so that snapshots, replicas and backups carry it. The name has
/// the form of a gocryptfs long-name side file: gocryptfs skips such names silently when it
/// lists a directory and `-fsck` does not report them (checked with 2.5.1 and 2.6.1), while
/// any other extra name would log "could not decrypt entry" on every listing.
pub const KEY_CHECK_FILE: &str = "gocryptfs.longname.rynas-key-check.name";
/// How long a lock waits for gocryptfs to end after the unmount.
const UNIT_STOP_WAIT: Duration = Duration::from_secs(5);
const CMD_TIMEOUT: Duration = Duration::from_secs(60);
/// Bytes of a gocryptfs master key.
const KEY_LEN: usize = 32;
/// The answer of a lock that unmounted the folder while gocryptfs still serves it elsewhere.
pub const IN_USE_ELSEWHERE: &str = "the folder is unmounted for rynas but gocryptfs still serves a copy of it to another mount namespace (a container or service started while it was unlocked): stop it, then lock again";

/// Why an operation failed. The texts never carry a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptError {
    /// Wrong password or recovery key.
    WrongPassword,
    /// Still in use: not locked.
    Busy(String),
    /// gocryptfs is not installed.
    NotInstalled,
    /// The request does not fit the folder (exists already, not an encrypted folder...).
    Invalid(String),
    /// A command failed.
    Failed(String),
}

impl std::fmt::Display for CryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongPassword => f.write_str("wrong password or recovery key"),
            Self::Busy(m) => write!(f, "the folder is in use: {m}"),
            Self::NotInstalled => f.write_str("gocryptfs is not installed"),
            Self::Invalid(m) | Self::Failed(m) => f.write_str(m),
        }
    }
}

impl From<String> for CryptError {
    fn from(s: String) -> Self {
        Self::Failed(s)
    }
}

// ---------------------------------------------------------------------------------------------
// layout and command lines (pure)
// ---------------------------------------------------------------------------------------------

/// `<parent>/.rynas-crypt/<name>` for the mount point `<parent>/<name>`; `None` for a path
/// that cannot be one (empty, `/`, a hidden name).
pub fn cipher_dir(path: &str) -> Option<String> {
    let p = path.trim_end_matches('/');
    let (parent, name) = p.rsplit_once('/')?;
    if !p.starts_with('/') || name.is_empty() || name.starts_with('.') || name == ".." {
        return None;
    }
    Some(format!("{parent}/{CRYPT_DIR}/{name}"))
}

/// The key-check file of a folder, inside its ciphertext root ([`KEY_CHECK_FILE`]).
fn check_file(cipher: &str) -> String {
    format!("{}/{KEY_CHECK_FILE}", cipher.trim_end_matches('/'))
}

/// Where the first release kept the key check: `<cipher>.check`, outside the ciphertext
/// subvolume (so snapshots and backups missed it).
fn legacy_check_file(cipher: &str) -> String {
    format!("{}.check", cipher.trim_end_matches('/'))
}

/// A key check as stored: 64 hex digits.
fn valid_check(s: &str) -> bool {
    let t = s.trim();
    t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Writes a key check into the ciphertext root (0600, never through a link).
fn write_check(cipher: &str, check: &str) -> std::io::Result<()> {
    let f = check_file(cipher);
    let tmp = format!("{f}.new");
    let _ = std::fs::remove_file(&tmp);
    let mut h = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    h.write_all(format!("{}\n", check.trim()).as_bytes())?;
    h.sync_all()?;
    std::fs::rename(&tmp, &f)
}

/// Moves a key check from its old place (`<cipher>.check`) into the ciphertext root; a
/// no-op when there is nothing to move.
pub fn migrate_check(cipher: &str, log: &dyn Fn(&str)) {
    let old = legacy_check_file(cipher);
    let Ok(m) = std::fs::symlink_metadata(&old) else { return };
    if !m.is_file() || !Path::new(cipher).join(CONF).is_file() {
        return;
    }
    if !Path::new(&check_file(cipher)).is_file() {
        match std::fs::read_to_string(&old) {
            Ok(c) if valid_check(&c) => {
                if let Err(e) = write_check(cipher, &c) {
                    log(&format!("warning: moving {old} into {cipher}: {e}"));
                    return;
                }
            }
            _ => {
                log(&format!("warning: {old} is not a key check: left alone"));
                return;
            }
        }
    }
    log(&format!("key check of {cipher} moved inside its ciphertext"));
    let _ = std::fs::remove_file(&old);
}

/// The stored key check of a folder (moved from its old place first).
fn read_check(cipher: &str, log: &dyn Fn(&str)) -> Option<String> {
    migrate_check(cipher, log);
    std::fs::read_to_string(check_file(cipher))
        .ok()
        .filter(|c| valid_check(c))
        .map(|c| c.trim().to_ascii_lowercase())
}

/// `systemd-escape --path` of `path`.
pub fn escape_path(path: &str) -> String {
    let p = path.trim_matches('/');
    let mut out = String::new();
    for (i, b) in p.bytes().enumerate() {
        match b {
            b'/' => out.push('-'),
            b'.' if i == 0 => out.push_str("\\x2e"),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b':' | b'_' | b'.' => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

/// The mount unit of the folder at `path` (no `.service`).
pub fn unit_name(path: &str) -> String {
    format!("{UNIT_PREFIX}-{}", escape_path(path))
}

/// A file-name-safe stem for the folder's credential and key files.
pub fn file_stem(path: &str) -> String {
    let h = Sha256::digest(path.trim_end_matches('/').as_bytes());
    let hex: String = h.iter().take(12).map(|b| format!("{b:02x}")).collect();
    format!("{UNIT_PREFIX}-{hex}")
}

/// A password rynas accepts: 8–256 characters (the UI checks the same), no control
/// characters (gocryptfs reads up to the newline).
pub fn check_password(p: &str) -> Result<(), String> {
    let n = p.chars().count();
    if n < PASSWORD_MIN {
        return Err(format!("the password needs at least {PASSWORD_MIN} characters"));
    }
    if n > PASSWORD_MAX {
        return Err(format!("the password is longer than {PASSWORD_MAX} characters"));
    }
    if p.chars().any(char::is_control) {
        return Err("the password cannot contain line breaks, tabs or other control characters".into());
    }
    Ok(())
}

/// 32 random bytes as gocryptfs prints a master key: 8 groups of 8 hex digits.
pub fn new_master_key() -> Secret {
    let mut b = [0u8; KEY_LEN];
    getrandom::fill(&mut b).expect("the operating system random source failed");
    let s = group_hex(&b);
    b.fill(0);
    s
}

fn group_hex(b: &[u8]) -> Secret {
    let mut s = String::with_capacity(KEY_LEN * 2 + 7);
    for (i, x) in b.iter().enumerate() {
        if i > 0 && i % 4 == 0 {
            s.push('-');
        }
        s.push_str(&format!("{x:02x}"));
    }
    Secret::new(s)
}

/// A recovery key as typed (dashes, spaces, any case) in gocryptfs' form; `None` when it is
/// not 64 hex digits.
pub fn normalize_master_key(k: &str) -> Option<Secret> {
    let hex: String = k
        .chars()
        .filter(|c| !matches!(c, '-' | ' ' | '\t' | '\n' | '\r'))
        .collect::<String>()
        .to_ascii_lowercase();
    let hex = Secret::new(hex);
    let h = hex.expose();
    if h.len() != KEY_LEN * 2 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0u8; KEY_LEN];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()?;
    }
    let s = group_hex(&bytes);
    bytes.fill(0);
    Some(s)
}

/// What rynas keeps to recognise the right recovery key: SHA-256 over a label and the key
/// bytes. A 256-bit random key cannot be found from it.
pub fn key_check(master: &Secret) -> String {
    let hex: String = master.expose().chars().filter(|c| *c != '-').collect();
    let hex = Secret::new(hex);
    let mut h = Sha256::new();
    h.update(b"rynas-crypt-key-check\0");
    h.update(hex.expose().as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// `gocryptfs -init` with our master key and the password on standard input.
///
/// Order checked live with gocryptfs 2.5.1 (Debian 13) and 2.6.1 (trixie-backports): `-init`
/// reads the **password first, then the master key** ("Reading Password from stdin",
/// "Reading Masterkey from stdin"). The other order fails with "Could not parse master key"
/// (exit 14), and a password made of 64 hex digits would silently swap the two roles.
pub fn init_cmd(cipher: &str, master: &Secret, password: &Secret) -> Cmd {
    Cmd::new(GOCRYPTFS)
        .args(["-init", "-masterkey", "stdin", "--", cipher])
        .stdin(format!("{}\n{}\n", password.expose(), master.expose()))
        .env_clear()
        .timeout(CMD_TIMEOUT)
}

/// `gocryptfs -passwd`: the current password (or with `recovery` the master key), then the
/// new password, on standard input. Checked live with 2.5.1 and 2.6.1: plain `-passwd` reads
/// the old password, then the new one; `-passwd -masterkey stdin` reads the master key, then
/// the new password.
pub fn passwd_cmd(cipher: &str, current: &Secret, recovery: bool, new: &Secret) -> Cmd {
    let mut c = Cmd::new(GOCRYPTFS).arg("-passwd");
    if recovery {
        c = c.args(["-masterkey", "stdin"]);
    }
    c.args(["--", cipher])
        .stdin(format!("{}\n{}\n", current.expose(), new.expose()))
        .env_clear()
        .timeout(CMD_TIMEOUT)
}

/// The gocryptfs command line of the mount unit; the password comes from the unit's
/// credential `pass`.
pub fn mount_argv(cipher: &str, mount: &str, btrfs: bool) -> Vec<String> {
    let mut v: Vec<String> = [GOCRYPTFS, "-allow_other", "-acl"].map(String::from).to_vec();
    if btrfs {
        // copy-on-write: preallocation guarantees nothing on btrfs and costs write speed
        v.push("-noprealloc".into());
    }
    v.extend(
        ["-passfile", "${CREDENTIALS_DIRECTORY}/pass", "--", cipher, mount].map(String::from),
    );
    v
}

/// `systemd-run` of the mount unit with the credential file `cred`.
pub fn mount_cmd(path: &str, cipher: &str, cred: &str, btrfs: bool) -> Cmd {
    let unit = unit_name(path);
    let props = [
        format!("LoadCredential=pass:{cred}"),
        "TimeoutStopSec=30".to_string(),
        format!("RequiresMountsFor={cipher}"),
    ];
    let props: Vec<&str> = props.iter().map(String::as_str).collect();
    Outside::new(&unit, "rynas: encrypted folder", CMD_TIMEOUT)
        .properties(&props)
        .daemon(mount_argv(cipher, path, btrfs))
}

/// The unmount of a lock: never lazy, so a folder in use stays mounted.
pub fn umount_cmd(path: &str) -> Cmd {
    Outside::new("rynas-crypt-lock", "rynas: lock an encrypted folder", Duration::from_secs(30))
        .cmd(["umount", path])
}

/// `chattr ±i` on the mount point (needs `CAP_LINUX_IMMUTABLE`: outside the sandbox).
pub fn immutable_cmd(path: &str, on: bool) -> Cmd {
    Outside::new("rynas-crypt-mountpoint", "rynas: encrypted folder mount point", Duration::from_secs(30))
        .cmd(["chattr", if on { "+i" } else { "-i" }, path])
}

/// `systemd-creds encrypt` of the password (standard input) into `file`, bound to `name`.
/// The key is named, never `auto`: `host+tpm2` exactly when [`has_tpm2_cmd`] said yes (what
/// the UI then promises), else `host` only.
pub fn seal_cmd(name: &str, file: &str, password: &Secret, tpm2: bool) -> Cmd {
    let key = if tpm2 { "--with-key=host+tpm2" } else { "--with-key=host" };
    Cmd::new("systemd-creds")
        .args(["encrypt", &format!("--name={name}"), key, "-", file])
        .stdin(password.expose().as_bytes().to_vec())
        .env_clear()
        .timeout(CMD_TIMEOUT)
}

/// `systemd-creds has-tpm2 --quiet`: exit 0 only with full TPM2 support (firmware, driver,
/// system and the libraries systemd needs), the test `systemd-creds` itself uses.
pub fn has_tpm2_cmd() -> Cmd {
    Cmd::new("systemd-creds").args(["has-tpm2", "--quiet"]).env_clear().timeout(Duration::from_secs(10))
}

/// `gocryptfs-xray -dumpmasterkey` of the folder's configuration, the password on standard
/// input (checked live with 2.5.1 and 2.6.1: it prints the key as 64 hex digits; a wrong
/// password prints "Password incorrect." and exits 8). Also the read-only password check:
/// unlike `-passwd` it does not rewrite `gocryptfs.conf`.
pub fn xray_cmd(cipher: &str, password: &Secret) -> Cmd {
    let conf = format!("{}/{CONF}", cipher.trim_end_matches('/'));
    Cmd::new(XRAY)
        .args(["-dumpmasterkey", "--", &conf])
        .stdin(format!("{}\n", password.expose()))
        .env_clear()
        .timeout(CMD_TIMEOUT)
}

/// `systemd-creds decrypt` of `file` to standard output.
pub fn unseal_cmd(name: &str, file: &str) -> Cmd {
    Cmd::new("systemd-creds")
        .args(["decrypt", &format!("--name={name}"), file, "-"])
        .env_clear()
        .timeout(CMD_TIMEOUT)
}

/// The exit status of the start command of a unit from `systemctl show -p ExecStart
/// --value` (`{ path=… ; … ; code=exited ; status=12 }`; for a forking service the start
/// command is not the main process, so `ExecMainStatus` stays 0) or from
/// `ExecMainStatus=`.
pub fn parse_exit_status(show: &str) -> Option<i32> {
    let i = show.rfind("status=")?;
    let digits: String = show[i + 7..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Unescapes the octal escapes of a mountinfo field (`\040` for a space).
fn unescape_mountinfo(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() {
            let oct = &b[i + 1..i + 4];
            if oct.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let oct = std::str::from_utf8(oct).unwrap_or("077");
                out.push(u8::from_str_radix(oct, 8).unwrap_or(b'?'));
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Is the last mount on `path` in this mountinfo text a gocryptfs one?
pub fn mounted_in(mountinfo: &str, path: &str) -> bool {
    let want = path.trim_end_matches('/');
    let mut last = None;
    for l in mountinfo.lines() {
        let Some((left, right)) = l.split_once(" - ") else { continue };
        let Some(mp) = left.split(' ').nth(4) else { continue };
        if unescape_mountinfo(mp) != want {
            continue;
        }
        last = right.split(' ').next().map(str::to_string);
    }
    last.is_some_and(|t| t == "fuse.gocryptfs")
}

/// `gocryptfs 2.5.1; go-fuse 2.4.2; …` (Debian 13) or `gocryptfs v2.6.1; go-fuse [vendored];
/// …` (upstream builds) → `2.5.1` / `2.6.1`.
pub fn parse_version(out: &str) -> Option<String> {
    let first = out.lines().next()?.split(';').next()?;
    let v = first.trim().strip_prefix("gocryptfs")?.trim();
    let v = v.strip_prefix('v').unwrap_or(v);
    (!v.is_empty()).then(|| v.to_string())
}

/// The version has `-init -masterkey` ([`MIN_VERSION`] or newer); an unreadable one is not
/// trusted.
pub fn version_supported(v: &str) -> bool {
    let mut it = v.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse::<u32>().unwrap_or(0));
    match (it.next(), it.next()) {
        (Some(major), minor) => (major, minor.unwrap_or(0)) >= MIN_VERSION,
        _ => false,
    }
}

/// The error of a gocryptfs too old for rynas.
fn too_old(v: &str) -> CryptError {
    CryptError::Invalid(format!(
        "gocryptfs {v} is too old: encrypted folders need {}.{} or newer (Debian 13 or newer ships it; on Debian 12 install a newer gocryptfs first)",
        MIN_VERSION.0, MIN_VERSION.1
    ))
}

/// The master key printed by `gocryptfs-xray -dumpmasterkey` in rynas' form (8 groups).
fn parse_xray_key(out: &str) -> Option<Secret> {
    let line = out.lines().map(str::trim).find(|l| l.len() >= 64 && l.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))?;
    normalize_master_key(line)
}

/// The end of a command's error output, for an error text (never a secret: gocryptfs and
/// systemd do not print them).
fn tail(o: &CmdOutput) -> String {
    let t = if o.stderr.trim().is_empty() { &o.stdout } else { &o.stderr };
    t.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(300)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// the backend trait
// ---------------------------------------------------------------------------------------------

/// What [`CryptHost::create`] made.
#[derive(Debug)]
pub struct Created {
    /// The recovery key (the master key).
    pub key: Secret,
    /// The folder exists and is initialised but the final unlock failed: it stays (locked)
    /// and is unlocked, or its recovery key shown again, with its password.
    pub unlock_error: Option<CryptError>,
}

/// Encrypted folders on a system. `path` is always the mount point `<place>/<name>`.
pub trait CryptHost: Send + Sync {
    fn info(&self) -> CryptInfo;
    fn state(&self, path: &str) -> CryptState;
    /// A sealed password exists: the folder is unlocked when rynasd starts.
    fn auto_unlock(&self, path: &str) -> bool;
    /// Creates, initialises and unlocks a new folder; returns its recovery key. Once the
    /// ciphertext is initialised the folder is kept even when the unlock fails.
    fn create(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<Created, CryptError>;
    fn unlock(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<(), CryptError>;
    /// Unmounts the folder. Refused while it is in use, including when gocryptfs keeps
    /// serving a copy of the mount held by another mount namespace ([`IN_USE_ELSEWHERE`]).
    fn lock(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError>;
    /// A new password from the current one, or from the recovery key with `recovery`.
    /// `Ok(Some(warning))`: the password changed but the automatic unlock had to be turned
    /// off (the new password could not be sealed).
    fn change_password(
        &self,
        path: &str,
        current: &Secret,
        recovery: bool,
        new: &Secret,
        log: &dyn Fn(&str),
    ) -> Result<Option<String>, CryptError>;
    /// `Some`: checks the password and seals it on this machine; `None`: forgets it.
    fn set_auto_unlock(&self, path: &str, password: Option<&Secret>, log: &dyn Fn(&str)) -> Result<(), CryptError>;
    /// The sealed password, when there is one.
    fn sealed_password(&self, path: &str) -> Result<Option<Secret>, CryptError>;
    /// The recovery key again, read from the folder's configuration with its password.
    fn recovery_key(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<Secret, CryptError>;
    /// Locks the folder and deletes its ciphertext, its mount point and its keys.
    fn remove(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError>;

    /// When rynasd starts: credential files a crash left behind are wiped, key checks of
    /// `paths` are moved into their ciphertext.
    fn at_start(&self, _paths: &[String], _log: &dyn Fn(&str)) {}

    /// The folders live in memory (tests, `--dry-system`): nothing is installed or mounted.
    fn simulated(&self) -> bool {
        false
    }

    /// A simulated install finished (no-op on the machine, where the package tells).
    fn mark_installed(&self) {}

    /// [`rynas_api_types::ShareStatus::crypt`] of an encrypted share.
    fn status(&self, path: &str) -> CryptStatus {
        CryptStatus {
            state: self.state(path),
            auto_unlock: self.auto_unlock(path),
            cipher_dir: cipher_dir(path).unwrap_or_default(),
        }
    }
}

fn need_cipher(path: &str) -> Result<String, CryptError> {
    cipher_dir(path).ok_or_else(|| CryptError::Invalid(format!("{path} cannot be an encrypted folder")))
}

// ---------------------------------------------------------------------------------------------
// the machine
// ---------------------------------------------------------------------------------------------

/// rynasd as root on the machine.
pub struct HostCrypt {
    /// Creates the ciphertext subvolume (the same creator as the shares').
    pub subvolumes: Option<Arc<dyn SubvolumeCreator>>,
    /// rynasd's state directory: sealed passwords in `<state>/crypt` (0700).
    pub state_dir: PathBuf,
    /// One change at a time (two unlocks of one folder, a lock during a create...).
    pub busy: Mutex<()>,
    /// `systemd-creds has-tpm2`, asked once.
    tpm2: std::sync::OnceLock<bool>,
}

impl HostCrypt {
    pub fn new(subvolumes: Option<Arc<dyn SubvolumeCreator>>, state_dir: &Path) -> Self {
        Self { subvolumes, state_dir: state_dir.to_path_buf(), busy: Mutex::new(()), tpm2: std::sync::OnceLock::new() }
    }

    fn key_file(&self, path: &str) -> PathBuf {
        self.state_dir.join("crypt").join(format!("{}.cred", file_stem(path)))
    }

    fn installed(&self) -> bool {
        Path::new(GOCRYPTFS).exists()
    }

    fn version(&self) -> Option<String> {
        Cmd::new(GOCRYPTFS)
            .arg("-version")
            .env_clear()
            .timeout(Duration::from_secs(10))
            .output_blocking()
            .ok()
            .and_then(|o| parse_version(&o.stdout))
    }

    fn has_tpm2(&self) -> bool {
        *self.tpm2.get_or_init(|| has_tpm2_cmd().output_blocking().is_ok_and(|o| o.success()))
    }

    fn mounted(&self, path: &str) -> bool {
        mounted_in(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default(), path)
    }

    fn systemctl(&self, args: &[&str]) -> Result<CmdOutput, String> {
        Cmd::new("systemctl")
            .args(args)
            .timeout(Duration::from_secs(30))
            .output_blocking()
            .map_err(|e| format!("systemctl: {e}"))
    }

    /// The folder's gocryptfs unit is running (activating or deactivating included). It
    /// outlives rynasd's view of the mount when another mount namespace holds a copy.
    fn unit_active(&self, path: &str) -> bool {
        let svc = format!("{}.service", unit_name(path));
        self.systemctl(&["is-active", "--quiet", &svc]).is_ok_and(|o| o.success())
    }

    /// Waits up to `wait` for the unit to end; `true` when it did.
    fn wait_inactive(&self, path: &str, wait: Duration) -> bool {
        let end = std::time::Instant::now() + wait;
        loop {
            if !self.unit_active(path) {
                return true;
            }
            if std::time::Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn run(&self, c: &Cmd, log: &dyn Fn(&str), what: &str) -> Result<CmdOutput, CryptError> {
        log(&format!("$ {}", c.display()));
        c.output_blocking().map_err(|e| CryptError::Failed(format!("{what}: {e}")))
    }

    /// The password opens the folder's configuration (read-only: [`xray_cmd`]); returns the
    /// master key.
    fn open_key(&self, cipher: &str, password: &Secret) -> Result<Secret, CryptError> {
        if !Path::new(XRAY).exists() {
            return Err(CryptError::Failed(format!("{XRAY} is missing: reinstall the gocryptfs package")));
        }
        let mut o = xray_cmd(cipher, password)
            .output_blocking()
            .map_err(|e| CryptError::Failed(format!("gocryptfs-xray: {e}")))?;
        let key = parse_xray_key(&o.stdout);
        wipe_string(&mut o.stdout);
        if o.success()
            && let Some(k) = key
        {
            return Ok(k);
        }
        if o.stderr.contains("Password incorrect") || o.code == Some(EXIT_WRONG_PASSWORD) {
            return Err(CryptError::WrongPassword);
        }
        Err(CryptError::Failed(format!("gocryptfs-xray: {}", tail(&o))))
    }

    fn unlock_inner(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        match self.state(path) {
            CryptState::Unlocked => return Ok(()),
            CryptState::Missing => return Err(CryptError::Invalid(format!("{path} is not an encrypted folder"))),
            CryptState::Locked => {}
            CryptState::InUse => {
                // gocryptfs of an earlier unlock still serves a copy held elsewhere (rynasd
                // refuses this while containers use the folder): the right password first,
                // then the stale instance goes and a fresh one mounts here
                let cipher = need_cipher(path)?;
                self.open_key(&cipher, password)?;
                let svc = format!("{}.service", unit_name(path));
                log(&format!("{svc} still runs without a mount for rynas: stopping it"));
                let _ = self.systemctl(&["stop", &svc]);
                if !self.wait_inactive(path, UNIT_STOP_WAIT) {
                    return Err(CryptError::Failed(format!("{svc} does not stop: see journalctl -u {svc}")));
                }
            }
        }
        if !self.installed() {
            return Err(CryptError::NotInstalled);
        }
        let cipher = need_cipher(path)?;
        let unit = unit_name(path);
        let cred = write_credential(path, password)?;
        let _ = self.systemctl(&["reset-failed", &format!("{unit}.service")]);
        let btrfs = std::fs::metadata(&cipher).is_ok_and(|m| m.ino() == 256);
        let cmd = mount_cmd(path, &cipher, &cred.to_string_lossy(), btrfs);
        log(&format!("$ {}", cmd.display()));
        let r = cmd.output_blocking();
        // the credential was copied into the unit (or the start failed): gone either way
        wipe(&cred);
        let ok = matches!(&r, Ok(o) if o.success());
        if ok {
            for _ in 0..40 {
                if self.mounted(path) {
                    log(&format!("{path} unlocked ({unit})"));
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            return Err(CryptError::Failed(format!("{unit} started but {path} is not mounted: see journalctl -u {unit}")));
        }
        let svc = format!("{unit}.service");
        let status = self
            .systemctl(&["show", "-p", "ExecStart", "--value", &svc])
            .ok()
            .and_then(|o| parse_exit_status(&o.stdout))
            .filter(|s| *s != 0)
            .or_else(|| {
                self.systemctl(&["show", "-p", "ExecMainStatus", &svc])
                    .ok()
                    .and_then(|o| parse_exit_status(&o.stdout.replace("ExecMainStatus=", "status=")))
            });
        let _ = self.systemctl(&["reset-failed", &svc]);
        if status == Some(EXIT_WRONG_PASSWORD) {
            return Err(CryptError::WrongPassword);
        }
        let detail = match r {
            Ok(o) => tail(&o),
            Err(e) => e.to_string(),
        };
        Err(CryptError::Failed(format!(
            "gocryptfs did not start (exit status {}): {detail}; see journalctl -u {unit}",
            status.map(|s| s.to_string()).unwrap_or_else(|| "?".into())
        )))
    }

    fn lock_inner(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        let svc = format!("{}.service", unit_name(path));
        match self.state(path) {
            CryptState::Unlocked => {}
            CryptState::Locked => return Ok(()),
            CryptState::Missing => return Err(CryptError::Invalid(format!("{path} is not an encrypted folder"))),
            // unmounted here, still served elsewhere: locked only once that copy is gone
            CryptState::InUse => {
                if self.wait_inactive(path, Duration::from_secs(1)) {
                    let _ = self.systemctl(&["reset-failed", &svc]);
                    log(&format!("{path} locked"));
                    return Ok(());
                }
                return Err(CryptError::Busy(IN_USE_ELSEWHERE.into()));
            }
        }
        let o = self.run(&umount_cmd(path), log, "umount")?;
        if !o.success() {
            let t = tail(&o);
            return Err(if t.to_ascii_lowercase().contains("busy") {
                CryptError::Busy(t)
            } else {
                CryptError::Failed(format!("umount {path}: {t}"))
            });
        }
        // gocryptfs ends by itself once the last copy of its filesystem is gone; a copy held
        // by another mount namespace (a container's bind mount is rprivate) keeps it running
        // and the plaintext reachable there: then the folder is not locked
        if !self.wait_inactive(path, UNIT_STOP_WAIT) {
            log(&format!("warning: {svc} still runs after the unmount: {IN_USE_ELSEWHERE}"));
            return Err(CryptError::Busy(IN_USE_ELSEWHERE.into()));
        }
        let _ = self.systemctl(&["reset-failed", &svc]);
        if self.mounted(path) {
            return Err(CryptError::Failed(format!("{path} is still mounted")));
        }
        log(&format!("{path} locked"));
        Ok(())
    }

    /// Deletes a ciphertext directory (subvolume, nested ones included, or plain folder) and
    /// its key check.
    fn delete_cipher(&self, cipher: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        if let Ok(m) = std::fs::symlink_metadata(cipher) {
            if m.file_type().is_symlink() {
                return Err(CryptError::Invalid(format!("{cipher} is a symbolic link: not deleted")));
            }
            if m.ino() == 256 {
                let c = Cmd::new("btrfs").args(["subvolume", "delete", "--recursive", cipher]);
                let o = self.run(&c, log, "btrfs")?;
                if !o.success() {
                    return Err(CryptError::Failed(format!("btrfs subvolume delete {cipher}: {}", tail(&o))));
                }
            } else {
                log(&format!("rm -r {cipher}"));
                std::fs::remove_dir_all(cipher).map_err(|e| CryptError::Failed(format!("{cipher}: {e}")))?;
            }
        }
        let _ = std::fs::remove_file(legacy_check_file(cipher));
        if let Some(parent) = Path::new(cipher).parent() {
            let _ = std::fs::remove_dir(parent);
        }
        Ok(())
    }

    fn remove_mountpoint(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        match std::fs::symlink_metadata(path) {
            Err(_) => Ok(()),
            Ok(m) if !m.is_dir() => Err(CryptError::Invalid(format!("{path} is not a folder: not deleted"))),
            Ok(_) => {
                let o = self.run(&immutable_cmd(path, false), log, "chattr")?;
                if !o.success() {
                    return Err(CryptError::Failed(format!("chattr -i {path}: {}", tail(&o))));
                }
                std::fs::remove_dir(path).map_err(|e| CryptError::Failed(format!("rmdir {path}: {e}")))
            }
        }
    }

    /// Seals `password` into the folder's credential file: written as `<file>.new`, then
    /// renamed over the old one, so an existing credential stays until the new one is there.
    fn seal(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        let file = self.key_file(path);
        let dir = file.parent().expect("key files live in a folder");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| CryptError::Failed(format!("{}: {e}", dir.display())))?;
        let tmp = file.with_extension("cred.new");
        let _ = std::fs::remove_file(&tmp);
        let t = tmp.to_string_lossy().into_owned();
        let o = self.run(&seal_cmd(&file_stem(path), &t, password, self.has_tpm2()), log, "systemd-creds")?;
        if !o.success() {
            let _ = std::fs::remove_file(&tmp);
            return Err(CryptError::Failed(format!("systemd-creds encrypt: {}", tail(&o))));
        }
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        std::fs::rename(&tmp, &file).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            CryptError::Failed(format!("{}: {e}", file.display()))
        })
    }
}

/// Overwrites a string's bytes with zeros (command output that held a key).
fn wipe_string(s: &mut String) {
    // SAFETY: zero bytes are valid UTF-8; the caller drops or reuses the string as empty.
    unsafe {
        for b in s.as_bytes_mut() {
            std::ptr::write_volatile(b, 0);
        }
    }
    s.clear();
}

/// Wipes the credential files a crash between their write and their wipe left in
/// [`RUN_DIR`].
pub fn wipe_leftovers(dir: &Path, log: &dyn Fn(&str)) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "pass") {
            log(&format!("wiping the leftover credential {}", p.display()));
            wipe(&p);
        }
    }
}

/// Writes the password to a fresh 0600 file in [`RUN_DIR`] (never following a link).
fn write_credential(path: &str, password: &Secret) -> Result<PathBuf, CryptError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(RUN_DIR)
        .map_err(|e| CryptError::Failed(format!("{RUN_DIR}: {e}")))?;
    let f = Path::new(RUN_DIR).join(format!("{}.pass", file_stem(path)));
    // a file left by a crash: replaced
    let _ = std::fs::remove_file(&f);
    let mut h = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&f)
        .map_err(|e| CryptError::Failed(format!("{}: {e}", f.display())))?;
    h.write_all(password.expose().as_bytes())
        .and_then(|_| h.write_all(b"\n"))
        .map_err(|e| CryptError::Failed(format!("{}: {e}", f.display())))?;
    Ok(f)
}

/// Overwrites a credential file with zeros and deletes it.
fn wipe(f: &Path) {
    if let Ok(m) = std::fs::symlink_metadata(f)
        && m.is_file()
        && let Ok(mut h) = std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(f)
    {
        let _ = h.write_all(&vec![0u8; m.len() as usize]);
        let _ = h.sync_all();
    }
    let _ = std::fs::remove_file(f);
}

impl CryptHost for HostCrypt {
    fn info(&self) -> CryptInfo {
        let installed = self.installed();
        let version = installed.then(|| self.version()).flatten();
        CryptInfo {
            installed,
            supported: version.as_deref().is_some_and(version_supported),
            version,
            tpm2: self.has_tpm2(),
        }
    }

    fn state(&self, path: &str) -> CryptState {
        let Some(cipher) = cipher_dir(path) else { return CryptState::Missing };
        if self.mounted(path) {
            CryptState::Unlocked
        } else if !Path::new(&cipher).join(CONF).is_file() {
            CryptState::Missing
        } else if self.unit_active(path) {
            CryptState::InUse
        } else {
            CryptState::Locked
        }
    }

    fn auto_unlock(&self, path: &str) -> bool {
        self.key_file(path).is_file()
    }

    fn create(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<Created, CryptError> {
        let _g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        check_password(password.expose()).map_err(CryptError::Invalid)?;
        if !self.installed() {
            return Err(CryptError::NotInstalled);
        }
        match self.version() {
            Some(v) if version_supported(&v) => {}
            v => return Err(too_old(v.as_deref().unwrap_or("of unknown version"))),
        }
        let cipher = need_cipher(path)?;
        if std::fs::symlink_metadata(path).is_ok() {
            return Err(CryptError::Invalid(format!("{path} already exists")));
        }
        if std::fs::symlink_metadata(&cipher).is_ok() {
            return Err(CryptError::Invalid(format!("{cipher} already exists")));
        }
        let parent = Path::new(&cipher).parent().expect("cipher dir has a parent");
        match std::fs::symlink_metadata(parent) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
                return Err(CryptError::Invalid(format!("{} is not a folder", parent.display())));
            }
            Ok(_) => {}
            Err(_) => {
                log(&format!("mkdir -m 700 {}", parent.display()));
                std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(parent)
                    .map_err(|e| CryptError::Failed(format!("{}: {e}", parent.display())))?;
            }
        }
        // ciphertext does not compress
        match &self.subvolumes {
            Some(c) => c.create(&cipher, false, false, log)?,
            None => {
                log(&format!("mkdir {cipher}"));
                std::fs::DirBuilder::new()
                    .mode(0o755)
                    .create(&cipher)
                    .map_err(|e| CryptError::Failed(format!("{cipher}: {e}")))?;
            }
        }
        let master = new_master_key();
        let undo = |log: &dyn Fn(&str)| {
            let _ = self.remove_mountpoint(path, log);
            let _ = self.delete_cipher(&cipher, log);
        };
        log(&format!("gocryptfs -init {cipher} (password and master key on standard input)"));
        let o = match init_cmd(&cipher, &master, password).output_blocking() {
            Ok(o) => o,
            Err(e) => {
                undo(log);
                return Err(CryptError::Failed(format!("gocryptfs -init: {e}")));
            }
        };
        if !o.success() {
            undo(log);
            return Err(CryptError::Failed(format!("gocryptfs -init: {}", tail(&o))));
        }
        if let Err(e) = write_check(&cipher, &key_check(&master)) {
            log(&format!("warning: {}: {e}", check_file(&cipher)));
        }
        log(&format!("mkdir -m 000 {path}"));
        if let Err(e) = std::fs::DirBuilder::new().mode(0o000).create(path) {
            undo(log);
            return Err(CryptError::Failed(format!("{path}: {e}")));
        }
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000));
        match self.run(&immutable_cmd(path, true), log, "chattr") {
            Ok(o) if o.success() => {}
            Ok(o) => {
                undo(log);
                return Err(CryptError::Failed(format!("chattr +i {path}: {}", tail(&o))));
            }
            Err(e) => {
                undo(log);
                return Err(e);
            }
        }
        // from here the folder exists for good: a failed unlock leaves it locked
        let unlock_error = self.unlock_inner(path, password, log).err();
        if let Some(e) = &unlock_error {
            log(&format!(
                "warning: {path} was created but not unlocked: {e}; unlock it with its password (its recovery key can be shown again with the password)"
            ));
        }
        Ok(Created { key: master, unlock_error })
    }

    fn unlock(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        let _g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        self.unlock_inner(path, password, log)
    }

    fn lock(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        let _g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        self.lock_inner(path, log)
    }

    fn change_password(
        &self,
        path: &str,
        current: &Secret,
        recovery: bool,
        new: &Secret,
        log: &dyn Fn(&str),
    ) -> Result<Option<String>, CryptError> {
        let _g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        check_password(new.expose()).map_err(CryptError::Invalid)?;
        if self.state(path) == CryptState::Missing {
            return Err(CryptError::Invalid(format!("{path} is not an encrypted folder")));
        }
        if !self.installed() {
            return Err(CryptError::NotInstalled);
        }
        let cipher = need_cipher(path)?;
        let current = if recovery {
            // -passwd -masterkey trusts any key: a wrong one would lock the files out for
            // good. The key check made at creation (or when the key was shown again) tells.
            let k = normalize_master_key(current.expose()).ok_or(CryptError::WrongPassword)?;
            let want = read_check(&cipher, log).ok_or_else(|| {
                CryptError::Invalid(format!(
                    "{path} has no key check (it was not made by rynas): show its recovery key once with the password, or reset the password with gocryptfs -passwd -masterkey"
                ))
            })?;
            if want != key_check(&k) {
                return Err(CryptError::WrongPassword);
            }
            k
        } else {
            current.clone()
        };
        log(&format!("gocryptfs -passwd {cipher} (passwords on standard input)"));
        let o = passwd_cmd(&cipher, &current, recovery, new)
            .output_blocking()
            .map_err(|e| CryptError::Failed(format!("gocryptfs -passwd: {e}")))?;
        if o.code == Some(EXIT_WRONG_PASSWORD) {
            return Err(CryptError::WrongPassword);
        }
        if !o.success() {
            return Err(CryptError::Failed(format!("gocryptfs -passwd: {}", tail(&o))));
        }
        // the backup is wrapped with the old password
        let bak = Path::new(&cipher).join(CONF_BAK);
        if bak.exists() {
            log(&format!("removing {}", bak.display()));
            let _ = std::fs::remove_file(&bak);
        }
        if !self.auto_unlock(path) {
            return Ok(None);
        }
        log("sealing the new password for the automatic unlock");
        match self.seal(path, new, log) {
            Ok(()) => Ok(None),
            Err(e) => {
                // the old credential holds the old password, which no longer opens the
                // folder: it goes, so the state says what happens at the next start
                let f = self.key_file(path);
                log(&format!("warning: the new password could not be sealed ({e}): removing {}", f.display()));
                let _ = std::fs::remove_file(&f);
                Ok(Some(format!("automatic unlock turned off: {e}")))
            }
        }
    }

    fn set_auto_unlock(&self, path: &str, password: Option<&Secret>, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        let _g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        let Some(p) = password else {
            let f = self.key_file(path);
            if f.exists() {
                log(&format!("removing {}", f.display()));
                std::fs::remove_file(&f).map_err(|e| CryptError::Failed(format!("{}: {e}", f.display())))?;
            }
            return Ok(());
        };
        if self.state(path) == CryptState::Missing {
            return Err(CryptError::Invalid(format!("{path} is not an encrypted folder")));
        }
        if !self.installed() {
            return Err(CryptError::NotInstalled);
        }
        // the password is checked without touching the folder's configuration
        let cipher = need_cipher(path)?;
        self.open_key(&cipher, p)?;
        self.seal(path, p, log)
    }

    fn sealed_password(&self, path: &str) -> Result<Option<Secret>, CryptError> {
        let f = self.key_file(path);
        if !f.is_file() {
            return Ok(None);
        }
        let mut o = unseal_cmd(&file_stem(path), &f.to_string_lossy())
            .output_blocking()
            .map_err(|e| CryptError::Failed(format!("systemd-creds decrypt: {e}")))?;
        if !o.success() {
            wipe_string(&mut o.stdout);
            return Err(CryptError::Failed(format!("systemd-creds decrypt: {}", tail(&o))));
        }
        let s = Secret::new(o.stdout.trim_end_matches('\n'));
        wipe_string(&mut o.stdout);
        Ok(Some(s))
    }

    fn recovery_key(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<Secret, CryptError> {
        if self.state(path) == CryptState::Missing {
            return Err(CryptError::Invalid(format!("{path} is not an encrypted folder")));
        }
        let cipher = need_cipher(path)?;
        let key = self.open_key(&cipher, password)?;
        // a folder without a key check (made outside rynas, or a failed write) gets one now,
        // so the recovery key can later reset the password
        if read_check(&cipher, log).is_none() {
            match write_check(&cipher, &key_check(&key)) {
                Ok(()) => log(&format!("key check written into {cipher}")),
                Err(e) => log(&format!("warning: key check of {cipher}: {e}")),
            }
        }
        Ok(key)
    }

    fn remove(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        let _g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        let cipher = need_cipher(path)?;
        self.lock_inner(path, log).or_else(|e| match e {
            // nothing to lock
            CryptError::Invalid(_) => Ok(()),
            e => Err(e),
        })?;
        // gocryptfs still running means the plaintext is reachable somewhere: keep it all
        if self.unit_active(path) {
            return Err(CryptError::Busy(IN_USE_ELSEWHERE.into()));
        }
        self.remove_mountpoint(path, log)?;
        self.delete_cipher(&cipher, log)?;
        let _ = std::fs::remove_file(self.key_file(path));
        Ok(())
    }

    fn at_start(&self, paths: &[String], log: &dyn Fn(&str)) {
        wipe_leftovers(Path::new(RUN_DIR), log);
        for p in paths {
            if let Some(c) = cipher_dir(p) {
                migrate_check(&c, log);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// in memory (tests, --dry-system)
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct MemFolder {
    password: Secret,
    master: Secret,
    unlocked: bool,
    sealed: bool,
    busy: bool,
    /// Unmounted here but still served elsewhere ([`CryptState::InUse`]).
    elsewhere: bool,
}

/// Encrypted folders kept in memory. With [`MemoryCrypt::with_shares`] the folders it creates
/// appear in that in-memory share host, so shares can be made on them.
#[derive(Default)]
pub struct MemoryCrypt {
    folders: Mutex<BTreeMap<String, MemFolder>>,
    installed: std::sync::atomic::AtomicBool,
    shares: Option<Arc<crate::host::MemoryShares>>,
    /// Sealing fails (a broken TPM, a full disk): tests of the password change.
    seal_fails: std::sync::atomic::AtomicBool,
    /// Every operation (`create /p`, `unlock /p`...), never with a secret.
    pub log: Mutex<Vec<String>>,
}

impl MemoryCrypt {
    pub fn new(installed: bool) -> Self {
        let m = Self::default();
        m.set_installed(installed);
        m
    }

    pub fn with_shares(mut self, shares: Arc<crate::host::MemoryShares>) -> Self {
        self.shares = Some(shares);
        self
    }

    pub fn set_installed(&self, on: bool) {
        self.installed.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// A locked folder at `path` with `password` (tests, demo data).
    pub fn add_locked(&self, path: &str, password: &str) {
        self.folders.lock().unwrap().insert(
            path.into(),
            MemFolder {
                password: Secret::new(password),
                master: new_master_key(),
                unlocked: false,
                sealed: false,
                busy: false,
                elsewhere: false,
            },
        );
        if let Some(s) = &self.shares {
            s.add_dir(path, None);
        }
    }

    /// Makes a lock of `path` fail as busy.
    pub fn set_busy(&self, path: &str, busy: bool) {
        if let Some(f) = self.folders.lock().unwrap().get_mut(path) {
            f.busy = busy;
        }
    }

    /// A lock of `path` unmounts it but gocryptfs keeps serving another namespace.
    pub fn set_elsewhere(&self, path: &str, on: bool) {
        if let Some(f) = self.folders.lock().unwrap().get_mut(path) {
            f.elsewhere = on;
        }
    }

    /// Sealing a password fails from now on.
    pub fn set_seal_fails(&self, on: bool) {
        self.seal_fails.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    fn note(&self, s: String) {
        self.log.lock().unwrap().push(s);
    }

    fn with<T>(&self, path: &str, f: impl FnOnce(&mut MemFolder) -> Result<T, CryptError>) -> Result<T, CryptError> {
        let mut m = self.folders.lock().unwrap();
        let x = m
            .get_mut(path)
            .ok_or_else(|| CryptError::Invalid(format!("{path} is not an encrypted folder")))?;
        f(x)
    }

    fn installed(&self) -> bool {
        self.installed.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl CryptHost for MemoryCrypt {
    fn simulated(&self) -> bool {
        true
    }

    fn mark_installed(&self) {
        self.set_installed(true);
    }

    fn info(&self) -> CryptInfo {
        CryptInfo {
            installed: self.installed(),
            version: self.installed().then(|| "2.5.1".into()),
            supported: self.installed(),
            tpm2: false,
        }
    }

    fn state(&self, path: &str) -> CryptState {
        match self.folders.lock().unwrap().get(path) {
            None => CryptState::Missing,
            Some(f) if f.unlocked => CryptState::Unlocked,
            Some(f) if f.elsewhere => CryptState::InUse,
            Some(_) => CryptState::Locked,
        }
    }

    fn auto_unlock(&self, path: &str) -> bool {
        self.folders.lock().unwrap().get(path).is_some_and(|f| f.sealed)
    }

    fn create(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<Created, CryptError> {
        check_password(password.expose()).map_err(CryptError::Invalid)?;
        if !self.installed() {
            return Err(CryptError::NotInstalled);
        }
        let cipher = need_cipher(path)?;
        let mut m = self.folders.lock().unwrap();
        if m.contains_key(path) || self.shares.as_ref().is_some_and(|s| crate::host::ShareHost::probe(s.as_ref(), path).exists) {
            return Err(CryptError::Invalid(format!("{path} already exists")));
        }
        let master = new_master_key();
        m.insert(
            path.into(),
            MemFolder {
                password: password.clone(),
                master: master.clone(),
                unlocked: true,
                sealed: false,
                busy: false,
                elsewhere: false,
            },
        );
        drop(m);
        if let Some(s) = &self.shares {
            s.add_dir(path, None);
        }
        log(&format!("created {path} (ciphertext {cipher}) and unlocked it (in memory)"));
        self.note(format!("create {path}"));
        Ok(Created { key: master, unlock_error: None })
    }

    fn unlock(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        self.with(path, |f| {
            if f.password != *password {
                return Err(CryptError::WrongPassword);
            }
            f.unlocked = true;
            f.elsewhere = false;
            Ok(())
        })?;
        log(&format!("{path} unlocked (in memory)"));
        self.note(format!("unlock {path}"));
        Ok(())
    }

    fn lock(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        self.with(path, |f| {
            if f.unlocked && f.busy {
                return Err(CryptError::Busy("target is busy".into()));
            }
            if f.elsewhere {
                f.unlocked = false;
                return Err(CryptError::Busy(IN_USE_ELSEWHERE.into()));
            }
            f.unlocked = false;
            Ok(())
        })?;
        log(&format!("{path} locked (in memory)"));
        self.note(format!("lock {path}"));
        Ok(())
    }

    fn change_password(
        &self,
        path: &str,
        current: &Secret,
        recovery: bool,
        new: &Secret,
        _log: &dyn Fn(&str),
    ) -> Result<Option<String>, CryptError> {
        check_password(new.expose()).map_err(CryptError::Invalid)?;
        let seal_fails = self.seal_fails.load(std::sync::atomic::Ordering::Relaxed);
        let warning = self.with(path, |f| {
            let ok = if recovery {
                normalize_master_key(current.expose()).is_some_and(|k| k == f.master)
            } else {
                f.password == *current
            };
            if !ok {
                return Err(CryptError::WrongPassword);
            }
            f.password = new.clone();
            if f.sealed && seal_fails {
                f.sealed = false;
                return Ok(Some("automatic unlock turned off: sealing failed (simulated)".to_string()));
            }
            Ok(None)
        })?;
        self.note(format!("password {path}"));
        Ok(warning)
    }

    fn set_auto_unlock(&self, path: &str, password: Option<&Secret>, _log: &dyn Fn(&str)) -> Result<(), CryptError> {
        let seal_fails = self.seal_fails.load(std::sync::atomic::Ordering::Relaxed);
        self.with(path, |f| {
            if let Some(p) = password {
                if f.password != *p {
                    return Err(CryptError::WrongPassword);
                }
                if seal_fails {
                    return Err(CryptError::Failed("systemd-creds encrypt: sealing failed (simulated)".into()));
                }
            }
            f.sealed = password.is_some();
            Ok(())
        })?;
        self.note(format!("auto-unlock {path} {}", password.is_some()));
        Ok(())
    }

    fn sealed_password(&self, path: &str) -> Result<Option<Secret>, CryptError> {
        Ok(self
            .folders
            .lock()
            .unwrap()
            .get(path)
            .filter(|f| f.sealed)
            .map(|f| f.password.clone()))
    }

    fn recovery_key(&self, path: &str, password: &Secret, _log: &dyn Fn(&str)) -> Result<Secret, CryptError> {
        let k = self.with(path, |f| {
            if f.password != *password {
                return Err(CryptError::WrongPassword);
            }
            Ok(f.master.clone())
        })?;
        self.note(format!("recovery key {path}"));
        Ok(k)
    }

    fn remove(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        self.lock(path, log).or_else(|e| match e {
            CryptError::Invalid(_) => Ok(()),
            e => Err(e),
        })?;
        self.folders.lock().unwrap().remove(path);
        if let Some(s) = &self.shares {
            s.state().paths.remove(path);
        }
        log(&format!("{path} and its ciphertext deleted (in memory)"));
        self.note(format!("remove {path}"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        assert_eq!(cipher_dir("/vol1/docs").as_deref(), Some("/vol1/.rynas-crypt/docs"));
        assert_eq!(cipher_dir("/vol1/docs/").as_deref(), Some("/vol1/.rynas-crypt/docs"));
        assert_eq!(
            cipher_dir("/srv/rynas/system/private").as_deref(),
            Some("/srv/rynas/system/.rynas-crypt/private")
        );
        assert_eq!(cipher_dir(""), None);
        assert_eq!(cipher_dir("/"), None);
        assert_eq!(cipher_dir("/vol1/.hidden"), None);
        assert_eq!(cipher_dir("relative/x"), None);
        assert_eq!(unit_name("/vol1/docs"), "rynas-crypt-vol1-docs");
        assert_eq!(unit_name("/srv/rynas/system/my-docs"), "rynas-crypt-srv-rynas-system-my\\x2ddocs");
        assert_eq!(escape_path("/a b/.c"), "a\\x20b-.c");
        assert_eq!(escape_path("/.x"), "\\x2ex");
        let s = file_stem("/vol1/docs");
        assert!(s.starts_with("rynas-crypt-") && s.len() == "rynas-crypt-".len() + 24, "{s}");
        assert_eq!(s, file_stem("/vol1/docs/"));
        assert_ne!(s, file_stem("/vol1/docs2"));
    }

    #[test]
    fn passwords() {
        assert!(check_password("correct horse").is_ok());
        assert!(check_password("short").is_err());
        assert!(check_password("eight\nchars").is_err());
        assert!(check_password("tab\tinside!").is_err());
        assert!(check_password(&"x".repeat(PASSWORD_MAX + 1)).is_err());
        assert!(check_password(&"x".repeat(PASSWORD_MAX)).is_ok());
        // characters, not bytes, for both limits (256 × 4 bytes stay below gocryptfs' 2048)
        assert!(check_password("ééééééé").is_err());
        assert!(check_password("éééééééé").is_ok());
        assert!(check_password(&"😀".repeat(PASSWORD_MAX)).is_ok());
        assert!(check_password(&"😀".repeat(PASSWORD_MAX + 1)).is_err());
    }

    #[test]
    fn versions_and_xray_output() {
        assert_eq!(parse_version("gocryptfs v2.6.1; go-fuse [vendored]; 2025-08-11 go1.24.6 linux/amd64").as_deref(), Some("2.6.1"));
        assert!(version_supported("2.5.1"));
        assert!(version_supported("2.6.1"));
        assert!(version_supported("3.0"));
        assert!(!version_supported("2.3"));
        assert!(!version_supported("2.4.0"));
        assert!(!version_supported("1.8.0"));
        assert!(!version_supported(""));
        assert!(matches!(too_old("2.3"), CryptError::Invalid(m) if m.contains("2.5 or newer")));
        let k = parse_xray_key("0123abcd0123abcd0123abcd0123abcd0123abcd0123abcd0123abcd0123abcd\n").unwrap();
        assert_eq!(k.expose(), "0123abcd-0123abcd-0123abcd-0123abcd-0123abcd-0123abcd-0123abcd-0123abcd");
        assert!(parse_xray_key("Password incorrect.\n").is_none());
    }

    #[test]
    fn key_check_lives_in_the_ciphertext_and_moves_there() {
        let d = tempfile::tempdir().unwrap();
        let cipher = d.path().join("docs");
        std::fs::create_dir(&cipher).unwrap();
        std::fs::write(cipher.join(CONF), "{}").unwrap();
        let c = cipher.to_string_lossy().into_owned();
        assert_eq!(check_file(&c), format!("{c}/{KEY_CHECK_FILE}"));
        let k = new_master_key();
        // the first release's place, outside the subvolume
        std::fs::write(legacy_check_file(&c), format!("{}\n", key_check(&k))).unwrap();
        let log = |_: &str| {};
        assert_eq!(read_check(&c, &log), Some(key_check(&k)));
        assert!(!Path::new(&legacy_check_file(&c)).exists());
        assert_eq!(std::fs::metadata(check_file(&c)).unwrap().permissions().mode() & 0o777, 0o600);
        // garbage is left alone and not trusted
        std::fs::remove_file(check_file(&c)).unwrap();
        std::fs::write(legacy_check_file(&c), "not a check").unwrap();
        assert_eq!(read_check(&c, &log), None);
        assert!(Path::new(&legacy_check_file(&c)).exists());
    }

    #[test]
    fn leftover_credentials_are_wiped() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("rynas-crypt-x.pass"), "secret\n").unwrap();
        std::fs::write(d.path().join("other.txt"), "keep").unwrap();
        wipe_leftovers(d.path(), &|_| {});
        assert!(!d.path().join("rynas-crypt-x.pass").exists());
        assert!(d.path().join("other.txt").exists());
    }

    #[test]
    fn master_keys() {
        let k = new_master_key();
        let s = k.expose();
        assert_eq!(s.len(), 64 + 7);
        assert_eq!(s.split('-').count(), 8);
        assert!(s.split('-').all(|g| g.len() == 8 && g.bytes().all(|b| b.is_ascii_hexdigit())));
        assert_ne!(new_master_key(), k);
        // typed back with spaces, upper case, no dashes
        let typed = s.replace('-', " ").to_ascii_uppercase();
        assert_eq!(normalize_master_key(&typed), Some(k.clone()));
        assert_eq!(normalize_master_key(&s.replace('-', "")), Some(k.clone()));
        assert_eq!(normalize_master_key("abc"), None);
        assert_eq!(normalize_master_key(&"g".repeat(64)), None);
        assert_eq!(key_check(&k), key_check(&normalize_master_key(&typed).unwrap()));
        assert_ne!(key_check(&k), key_check(&new_master_key()));
        assert_eq!(key_check(&k).len(), 64);
        assert!(!key_check(&k).contains(&s.replace('-', "")[..16]));
        // never printed
        assert_eq!(format!("{k:?}"), "<secret>");
    }

    #[test]
    fn secrets_stay_off_the_command_lines() {
        let pw = Secret::new("correct horse battery");
        let mk = new_master_key();
        let init = init_cmd("/vol1/.rynas-crypt/docs", &mk, &pw);
        let d = init.display();
        assert_eq!(d, "/usr/bin/gocryptfs -init -masterkey stdin -- /vol1/.rynas-crypt/docs");
        assert!(!d.contains("horse") && !d.contains(mk.expose()));
        // password first, then the master key (checked live with gocryptfs 2.5.1 and 2.6.1)
        assert_eq!(init.stdin_data().unwrap(), format!("correct horse battery\n{}\n", mk.expose()).as_bytes());
        let dbg = format!("{init:?}");
        assert!(!dbg.contains("horse") && !dbg.contains(mk.expose()), "{dbg}");

        let new = Secret::new("new password here");
        let p = passwd_cmd("/c", &pw, false, &new);
        assert_eq!(p.display(), "/usr/bin/gocryptfs -passwd -- /c");
        assert_eq!(p.stdin_data().unwrap(), b"correct horse battery\nnew password here\n");
        let r = passwd_cmd("/c", &mk, true, &new);
        assert_eq!(r.display(), "/usr/bin/gocryptfs -passwd -masterkey stdin -- /c");
        assert!(!r.display().contains(mk.expose()));
        // the master key, then the new password
        assert_eq!(r.stdin_data().unwrap(), format!("{}\nnew password here\n", mk.expose()).as_bytes());
        let x = xray_cmd("/vol1/.rynas-crypt/docs", &pw);
        assert_eq!(x.display(), "/usr/bin/gocryptfs-xray -dumpmasterkey -- /vol1/.rynas-crypt/docs/gocryptfs.conf");
        assert_eq!(x.stdin_data().unwrap(), b"correct horse battery\n");

        let m = mount_cmd("/vol1/docs", "/vol1/.rynas-crypt/docs", "/run/rynas/crypt/x.pass", true);
        let d = m.display();
        assert!(d.starts_with("systemd-run --quiet --service-type=forking --unit=rynas-crypt-vol1-docs "), "{d}");
        assert!(d.contains("--property=LoadCredential=pass:/run/rynas/crypt/x.pass"), "{d}");
        assert!(d.ends_with(
            "-- /usr/bin/gocryptfs -allow_other -acl -noprealloc -passfile ${CREDENTIALS_DIRECTORY}/pass -- /vol1/.rynas-crypt/docs /vol1/docs"
        ), "{d}");
        assert!(m.stdin_data().is_none());
        let plain = mount_argv("/c", "/m", false);
        assert!(!plain.contains(&"-noprealloc".to_string()));

        let u = umount_cmd("/vol1/docs").display();
        assert!(u.contains("systemd-run --wait --pipe") && u.ends_with("-- umount /vol1/docs"), "{u}");
        assert!(!u.contains("-l "), "never lazy: {u}");
        assert!(immutable_cmd("/vol1/docs", true).display().ends_with("-- chattr +i /vol1/docs"));
        assert!(immutable_cmd("/vol1/docs", false).display().ends_with("-- chattr -i /vol1/docs"));

        let s = seal_cmd("rynas-crypt-x", "/var/lib/rynas/crypt/x.cred", &pw, false);
        assert_eq!(s.display(), "systemd-creds encrypt --name=rynas-crypt-x --with-key=host - /var/lib/rynas/crypt/x.cred");
        assert!(seal_cmd("n", "/f", &pw, true).display().contains(" --with-key=host+tpm2 "));
        assert_eq!(has_tpm2_cmd().display(), "systemd-creds has-tpm2 --quiet");
        assert_eq!(s.stdin_data().unwrap(), b"correct horse battery");
        assert_eq!(
            unseal_cmd("rynas-crypt-x", "/f").display(),
            "systemd-creds decrypt --name=rynas-crypt-x /f -"
        );
    }

    #[test]
    fn exit_status_and_version() {
        let show = "{ path=/usr/bin/gocryptfs ; argv[]=/usr/bin/gocryptfs -allow_other ; ignore_errors=no ; start_time=[n/a] ; stop_time=[Sun 2026-11-08] ; pid=4242 ; code=exited ; status=12 }";
        assert_eq!(parse_exit_status(show), Some(12));
        assert_eq!(parse_exit_status("status=0/SUCCESS"), Some(0));
        assert_eq!(parse_exit_status("ExecMainStatus=1".replace("ExecMainStatus=", "status=").as_str()), Some(1));
        assert_eq!(parse_exit_status(""), None);
        assert_eq!(
            parse_version("gocryptfs 2.5.1; go-fuse 2.5.1; 2025-01-23 go1.24.1 linux/amd64\n").as_deref(),
            Some("2.5.1")
        );
        assert_eq!(parse_version("something else"), None);
    }

    #[test]
    fn mountinfo() {
        let mi = "\
36 25 0:31 / /vol1 rw,noatime shared:1 - btrfs /dev/md0 rw,space_cache=v2,subvolid=5,subvol=/
90 36 0:52 / /vol1/docs rw,nosuid,nodev,relatime shared:50 - fuse.gocryptfs /vol1/.rynas-crypt/docs rw,user_id=0,group_id=0,default_permissions,allow_other
91 36 0:53 / /vol1/my\\040files rw shared:51 - fuse.gocryptfs /vol1/.rynas-crypt/my\\040files rw
92 36 0:54 / /vol1/other rw shared:52 - fuse.sshfs host: rw
";
        assert!(mounted_in(mi, "/vol1/docs"));
        assert!(mounted_in(mi, "/vol1/docs/"));
        assert!(mounted_in(mi, "/vol1/my files"));
        assert!(!mounted_in(mi, "/vol1"));
        assert!(!mounted_in(mi, "/vol1/other"));
        assert!(!mounted_in(mi, "/vol1/none"));
        // something else mounted over it later wins
        let over = format!("{mi}93 90 0:60 / /vol1/docs rw - tmpfs tmpfs rw\n");
        assert!(!mounted_in(&over, "/vol1/docs"));
    }

    #[test]
    fn memory_state_machine() {
        let m = MemoryCrypt::new(false);
        let pw = Secret::new("correct horse");
        let log = |_: &str| {};
        assert!(matches!(m.create("/vol1/docs", &pw, &log), Err(CryptError::NotInstalled)));
        m.set_installed(true);
        assert!(matches!(m.create("/vol1/docs", &Secret::new("short"), &log), Err(CryptError::Invalid(_))));
        let created = m.create("/vol1/docs", &pw, &log).unwrap();
        assert!(created.unlock_error.is_none());
        let key = created.key;
        assert!(matches!(m.create("/vol1/docs", &pw, &log), Err(CryptError::Invalid(_))));
        // the recovery key again, with the password only
        assert_eq!(m.recovery_key("/vol1/docs", &Secret::new("wrong password"), &log), Err(CryptError::WrongPassword));
        assert_eq!(m.recovery_key("/vol1/docs", &pw, &log).unwrap(), key);
        // a copy served elsewhere: never reported as locked
        m.set_elsewhere("/vol1/docs", true);
        assert!(matches!(m.lock("/vol1/docs", &log), Err(CryptError::Busy(e)) if e == IN_USE_ELSEWHERE));
        assert_eq!(m.state("/vol1/docs"), CryptState::InUse);
        m.unlock("/vol1/docs", &pw, &log).unwrap();
        assert_eq!(m.state("/vol1/docs"), CryptState::Unlocked);
        assert_eq!(m.state("/vol1/docs"), CryptState::Unlocked);
        assert_eq!(m.state("/vol1/other"), CryptState::Missing);
        m.set_busy("/vol1/docs", true);
        assert!(matches!(m.lock("/vol1/docs", &log), Err(CryptError::Busy(_))));
        m.set_busy("/vol1/docs", false);
        m.lock("/vol1/docs", &log).unwrap();
        assert_eq!(m.state("/vol1/docs"), CryptState::Locked);
        m.lock("/vol1/docs", &log).unwrap();
        assert_eq!(m.unlock("/vol1/docs", &Secret::new("wrong password"), &log), Err(CryptError::WrongPassword));
        m.unlock("/vol1/docs", &pw, &log).unwrap();
        // password change with the recovery key, then the old password fails
        let new = Secret::new("brand new password");
        assert_eq!(
            m.change_password("/vol1/docs", &Secret::new("0".repeat(64)), true, &new, &log),
            Err(CryptError::WrongPassword)
        );
        let typed = Secret::new(key.expose().replace('-', " "));
        m.change_password("/vol1/docs", &typed, true, &new, &log).unwrap();
        m.lock("/vol1/docs", &log).unwrap();
        assert_eq!(m.unlock("/vol1/docs", &pw, &log), Err(CryptError::WrongPassword));
        assert_eq!(m.set_auto_unlock("/vol1/docs", Some(&pw), &log), Err(CryptError::WrongPassword));
        m.set_auto_unlock("/vol1/docs", Some(&new), &log).unwrap();
        assert!(m.auto_unlock("/vol1/docs"));
        assert_eq!(m.sealed_password("/vol1/docs").unwrap(), Some(new.clone()));
        let st = m.status("/vol1/docs");
        assert_eq!(st.state, CryptState::Locked);
        assert!(st.auto_unlock);
        assert_eq!(st.cipher_dir, "/vol1/.rynas-crypt/docs");
        // a password change whose seal fails: changed, with a warning, auto-unlock off
        m.set_seal_fails(true);
        let newer = Secret::new("newer password!");
        let w = m.change_password("/vol1/docs", &new, false, &newer, &log).unwrap();
        assert!(w.unwrap().starts_with("automatic unlock turned off"));
        assert!(!m.auto_unlock("/vol1/docs"));
        m.set_seal_fails(false);
        assert_eq!(m.change_password("/vol1/docs", &newer, false, &new, &log), Ok(None));
        m.set_auto_unlock("/vol1/docs", None, &log).unwrap();
        assert_eq!(m.sealed_password("/vol1/docs").unwrap(), None);
        m.remove("/vol1/docs", &log).unwrap();
        assert_eq!(m.state("/vol1/docs"), CryptState::Missing);
        let ops = m.log.lock().unwrap().join(",");
        assert!(!ops.contains("horse") && !ops.contains("password here"), "{ops}");
    }

    #[test]
    fn credential_file_is_private_and_wiped() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("x.pass");
        {
            let mut h = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&f).unwrap();
            h.write_all(b"secret\n").unwrap();
        }
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        wipe(&f);
        assert!(!f.exists());
        // a link is never followed
        let target = d.path().join("target");
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, &f).unwrap();
        wipe(&f);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    }

    /// The command lines against the real gocryptfs, in user space and a temporary folder
    /// (`cargo test -p rynas-services -- --ignored live_gocryptfs`; needs /usr/bin/gocryptfs).
    #[test]
    #[ignore]
    fn live_gocryptfs_secret_order() {
        if !Path::new(GOCRYPTFS).exists() || !Path::new(XRAY).exists() {
            eprintln!("gocryptfs not installed: skipped");
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let c = d.path().join("c");
        std::fs::create_dir(&c).unwrap();
        let c = c.to_string_lossy().into_owned();
        let (pw, mk) = (Secret::new("correct horse battery"), new_master_key());
        let o = init_cmd(&c, &mk, &pw).output_blocking().unwrap();
        assert!(o.success(), "{o:?}");
        let host = HostCrypt::new(None, d.path());
        assert_eq!(host.open_key(&c, &pw).unwrap(), mk);
        assert_eq!(host.open_key(&c, &Secret::new("wrong password")), Err(CryptError::WrongPassword));
        let new = Secret::new("new password here");
        let o = passwd_cmd(&c, &pw, false, &new).output_blocking().unwrap();
        assert!(o.success(), "{o:?}");
        assert_eq!(host.open_key(&c, &new).unwrap(), mk);
        let o = passwd_cmd(&c, &new, false, &pw).output_blocking().unwrap();
        assert!(o.success());
        let o = passwd_cmd(&c, &Secret::new("wrong password"), false, &new).output_blocking().unwrap();
        assert_eq!(o.code, Some(EXIT_WRONG_PASSWORD));
        // the recovery path: the master key, then the new password
        let third = Secret::new("third password!");
        let o = passwd_cmd(&c, &mk, true, &third).output_blocking().unwrap();
        assert!(o.success(), "{o:?}");
        assert_eq!(host.open_key(&c, &third).unwrap(), mk);
        assert!(host.info().supported);
    }
}
